#!/usr/bin/env python3
"""Materialize Raven's pinned external Stan and JAGS diagnostic corpora.

The committed manifests contain provenance and immutable archive hashes, but no
third-party model source. This tool downloads into ``target/``, verifies before
extracting, accounts for every discovered candidate, and writes a deterministic
index consumed by the external oracle and Rust integration tests.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sys
import tarfile
import tempfile
from string import Formatter
from typing import Any, Iterable
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFESTS = (
    REPO_ROOT / "crates/raven/tests/fixtures/diagnostic_corpora/stan.json",
    REPO_ROOT / "crates/raven/tests/fixtures/diagnostic_corpora/jags.json",
)
DEFAULT_ROOT = REPO_ROOT / "target/diagnostic-corpora"
SCHEMA_VERSION = 1
CHUNK_SIZE = 1024 * 1024


class CorpusError(RuntimeError):
    """A deterministic corpus validation or materialization failure."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(CHUNK_SIZE), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_bytes(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CorpusError(f"cannot read manifest {path}: {error}") from error
    validate_manifest(path, manifest)
    return manifest


def require_string(record: dict[str, Any], key: str, context: str) -> str:
    value = record.get(key)
    if not isinstance(value, str) or not value.strip():
        raise CorpusError(f"{context}.{key} must be a non-empty string")
    return value


def safe_relative_posix_path(value: str, context: str) -> PurePosixPath:
    if "\\" in value:
        raise CorpusError(f"{context} must use POSIX separators")
    raw_parts = value.split("/")
    path = PurePosixPath(value)
    if (
        path.is_absolute()
        or not raw_parts
        or any(part in {"", ".", ".."} for part in raw_parts)
    ):
        raise CorpusError(f"{context} must be a safe relative POSIX path")
    return path


def safe_glob(value: str, context: str) -> str:
    safe_relative_posix_path(value, context)
    return value


def safe_metadata_template(value: str, context: str) -> str:
    try:
        fields = list(Formatter().parse(value))
    except ValueError as error:
        raise CorpusError(f"{context} is not a valid path template: {error}") from error
    replacements = [field for _, field, _, _ in fields if field is not None]
    if replacements != ["stem"] or any(
        conversion or format_spec
        for _, field, format_spec, conversion in fields
        if field is not None
    ):
        raise CorpusError(f"{context} must contain exactly one plain {{stem}} field")
    safe_relative_posix_path(value.replace("{stem}", "placeholder"), context)
    return value


def validate_manifest(path: Path, manifest: dict[str, Any]) -> None:
    if manifest.get("schema_version") != SCHEMA_VERSION:
        raise CorpusError(f"{path}: schema_version must be {SCHEMA_VERSION}")
    language = require_string(manifest, "language", str(path))
    if language not in {"stan", "jags"}:
        raise CorpusError(f"{path}: unsupported language {language!r}")
    if manifest.get("suite") != "external-no-false-positive":
        raise CorpusError(f"{path}: unexpected suite")
    sources = manifest.get("sources")
    if not isinstance(sources, list) or not sources:
        raise CorpusError(f"{path}: sources must be a non-empty list")

    source_ids: set[str] = set()
    for source_index, source in enumerate(sources):
        context = f"{path}: sources[{source_index}]"
        if not isinstance(source, dict):
            raise CorpusError(f"{context} must be an object")
        source_id = require_string(source, "id", context)
        if re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", source_id) is None:
            raise CorpusError(
                f"{context}.id must be one safe filename component containing only "
                "letters, digits, dots, underscores, and hyphens"
            )
        if source_id in source_ids:
            raise CorpusError(f"{path}: duplicate source id {source_id!r}")
        source_ids.add(source_id)
        require_string(source, "project", context)
        require_string(source, "version", context)
        require_string(source, "revision", context)
        archive_url = require_string(source, "archive_url", context)
        if not archive_url.startswith("https://"):
            raise CorpusError(f"{context}.archive_url must use https")
        archive_hash = require_string(source, "archive_sha256", context)
        if len(archive_hash) != 64 or any(ch not in "0123456789abcdef" for ch in archive_hash):
            raise CorpusError(f"{context}.archive_sha256 must be lowercase SHA-256")
        archive_root = require_string(source, "archive_root", context)
        safe_relative_posix_path(archive_root, f"{context}.archive_root")
        if source.get("redistribution") != "fetch-only":
            raise CorpusError(f"{context}.redistribution must be fetch-only")
        license_record = source.get("license")
        if not isinstance(license_record, dict):
            raise CorpusError(f"{context}.license must be an object")
        require_string(license_record, "spdx", f"{context}.license")
        require_string(license_record, "evidence_url", f"{context}.license")
        discoveries = source.get("discovery")
        if not isinstance(discoveries, list) or not discoveries:
            raise CorpusError(f"{context}.discovery must be a non-empty list")
        for discovery_index, discovery in enumerate(discoveries):
            discovery_context = f"{context}.discovery[{discovery_index}]"
            if not isinstance(discovery, dict):
                raise CorpusError(f"{discovery_context} must be an object")
            discovery_type = require_string(discovery, "type", discovery_context)
            if discovery_type not in {"files", "qmd-fences"}:
                raise CorpusError(f"{discovery_context}: unsupported type {discovery_type!r}")
            globs = discovery.get("globs")
            if not isinstance(globs, list) or not globs or not all(
                isinstance(item, str) and item for item in globs
            ):
                raise CorpusError(f"{discovery_context}.globs must be non-empty strings")
            for glob_index, pattern in enumerate(globs):
                safe_glob(pattern, f"{discovery_context}.globs[{glob_index}]")
            expected_count = discovery.get("expected_count")
            if not isinstance(expected_count, int) or expected_count <= 0:
                raise CorpusError(f"{discovery_context}.expected_count must be positive")
            require_string(discovery, "kind", discovery_context)
            raven_mode = require_string(discovery, "raven_mode", discovery_context)
            if raven_mode not in {"all", "syntax-only", "oracle-classified"}:
                raise CorpusError(f"{discovery_context}: unsupported raven_mode {raven_mode!r}")
            require_string(discovery, "oracle_mode", discovery_context)
            metadata = discovery.get("metadata")
            if metadata is not None:
                if not isinstance(metadata, dict):
                    raise CorpusError(f"{discovery_context}.metadata must be an object")
                path_template = require_string(metadata, "path_template", f"{discovery_context}.metadata")
                safe_metadata_template(
                    path_template,
                    f"{discovery_context}.metadata.path_template",
                )
                require_string(metadata, "license_field", f"{discovery_context}.metadata")
            if discovery_type == "qmd-fences":
                require_string(discovery, "fence_language", discovery_context)


def archive_path(root: Path, source: dict[str, Any]) -> Path:
    suffix = ".tar.gz" if source["archive_url"].endswith((".tar.gz", "/tarball")) else ".archive"
    return root / "downloads" / f'{source["archive_sha256"]}{suffix}'


def download_archive(source: dict[str, Any], destination: Path, offline: bool) -> None:
    expected = source["archive_sha256"]
    if destination.is_file():
        actual = sha256_file(destination)
        if actual == expected:
            return
        destination.unlink()
        if offline:
            raise CorpusError(
                f'{source["id"]}: cached archive hash mismatch in offline mode '
                f"(expected {expected}, got {actual})"
            )
    if offline:
        raise CorpusError(f'{source["id"]}: verified archive is not cached')

    destination.parent.mkdir(parents=True, exist_ok=True)
    request = Request(
        source["archive_url"],
        headers={"User-Agent": "raven-diagnostic-corpus/1"},
    )
    try:
        with urlopen(request, timeout=60) as response, tempfile.NamedTemporaryFile(
            dir=destination.parent,
            prefix=f'.{source["id"]}-',
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            try:
                while chunk := response.read(CHUNK_SIZE):
                    temporary.write(chunk)
            except BaseException:
                temporary_path.unlink(missing_ok=True)
                raise
    except (HTTPError, URLError, TimeoutError, OSError) as error:
        raise CorpusError(f'{source["id"]}: download failed: {error}') from error

    actual = sha256_file(temporary_path)
    if actual != expected:
        temporary_path.unlink(missing_ok=True)
        raise CorpusError(
            f'{source["id"]}: archive hash mismatch (expected {expected}, got {actual})'
        )
    os.replace(temporary_path, destination)


def safe_member_path(name: str) -> PurePosixPath:
    normalized = name.rstrip("/")
    if not normalized:
        raise CorpusError("empty archive path")
    return safe_relative_posix_path(normalized, f"archive path {name!r}")


def extract_archive(source: dict[str, Any], archive: Path, destination: Path) -> Path:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(
        dir=destination.parent,
        prefix=f'.{source["id"]}-extract-',
    ))
    backup: Path | None = None
    seen: set[str] = set()
    seen_casefolded: dict[str, str] = {}
    try:
        with tarfile.open(archive, mode="r:*") as bundle:
            members = bundle.getmembers()
            for member in members:
                path = safe_member_path(member.name)
                if any(part.startswith(".") for part in path.parts[1:]):
                    continue
                normalized = path.as_posix()
                if normalized in seen:
                    raise CorpusError(f"duplicate archive path: {normalized}")
                seen.add(normalized)
                folded = normalized.casefold()
                previous = seen_casefolded.get(folded)
                if previous is not None and previous != normalized:
                    raise CorpusError(
                        f"case-folding archive collision: {previous!r} and {normalized!r}"
                    )
                seen_casefolded[folded] = normalized
                if member.issym() or member.islnk():
                    raise CorpusError(f"archive links are not allowed: {normalized}")
                if not (member.isdir() or member.isfile()):
                    raise CorpusError(f"unsupported archive entry: {normalized}")
            for member in members:
                relative = safe_member_path(member.name)
                if any(part.startswith(".") for part in relative.parts[1:]):
                    continue
                output = temporary.joinpath(*relative.parts)
                if output.parent != temporary and temporary not in output.parents:
                    raise CorpusError(f"archive path escapes extraction root: {member.name!r}")
                if member.isdir():
                    output.mkdir(parents=True, exist_ok=True)
                    continue
                output.parent.mkdir(parents=True, exist_ok=True)
                extracted = bundle.extractfile(member)
                if extracted is None:
                    raise CorpusError(f"cannot read archive entry: {member.name}")
                with extracted, output.open("xb") as target:
                    shutil.copyfileobj(extracted, target)

        archive_root = safe_relative_posix_path(
            source["archive_root"], f'{source["id"]}.archive_root'
        )
        extracted_root = temporary.joinpath(*archive_root.parts)
        if not extracted_root.is_dir():
            raise CorpusError(
                f'{source["id"]}: archive root {source["archive_root"]!r} is missing'
            )

        # Extract afresh from the verified archive on every materialization. The
        # extracted tree is mutable input, so never trust a marker from an older run.
        if destination.exists() or destination.is_symlink():
            backup = temporary.with_name(f"{temporary.name}-replaced")
            os.replace(destination, backup)
        try:
            os.replace(temporary, destination)
        except OSError:
            if backup is not None:
                os.replace(backup, destination)
                backup = None
            raise
        if backup is not None:
            if backup.is_dir() and not backup.is_symlink():
                shutil.rmtree(backup)
            else:
                backup.unlink()
            backup = None
        return destination.joinpath(*archive_root.parts)
    except CorpusError:
        shutil.rmtree(temporary, ignore_errors=True)
        if backup is not None and not destination.exists():
            os.replace(backup, destination)
        raise
    except (tarfile.TarError, OSError) as error:
        shutil.rmtree(temporary, ignore_errors=True)
        if backup is not None and not destination.exists():
            os.replace(backup, destination)
        raise CorpusError(f'{source["id"]}: archive extraction failed: {error}') from error


def matched_files(source_root: Path, patterns: Iterable[str]) -> list[Path]:
    paths: set[Path] = set()
    for pattern in patterns:
        paths.update(path for path in source_root.glob(pattern) if path.is_file())
    return sorted(paths, key=lambda path: path.relative_to(source_root).as_posix())


def copy_case(
    *,
    source: dict[str, Any],
    original: Path,
    relative_path: str,
    destination_root: Path,
    discovery: dict[str, Any],
    case_suffix: str | None = None,
    line_start: int | None = None,
    content: bytes | None = None,
) -> dict[str, Any]:
    case_id = f'{source["id"]}:{relative_path}'
    if case_suffix:
        case_id += case_suffix
    extension = ".stan" if discovery["type"] == "qmd-fences" else original.suffix
    digest = sha256_bytes(content) if content is not None else sha256_file(original)
    file_name = f"{digest[:16]}-{Path(relative_path).stem}{extension}"
    destination = destination_root / source["id"] / file_name
    destination.parent.mkdir(parents=True, exist_ok=True)
    if content is None:
        shutil.copyfile(original, destination)
    else:
        destination.write_bytes(content)
    record: dict[str, Any] = {
        "id": case_id,
        "language": "stan" if extension == ".stan" else "jags",
        "source_id": source["id"],
        "upstream_path": relative_path,
        "materialized_path": destination.relative_to(destination_root.parent).as_posix(),
        "sha256": digest,
        "kind": discovery["kind"],
        "raven_mode": discovery["raven_mode"],
        "oracle_mode": discovery["oracle_mode"],
    }
    if line_start is not None:
        record["line_start"] = line_start
    return record


def attach_case_metadata(
    record: dict[str, Any],
    source_root: Path,
    original: Path,
    discovery: dict[str, Any],
) -> None:
    metadata_spec = discovery.get("metadata")
    if metadata_spec is None:
        return
    relative_metadata = metadata_spec["path_template"].format(stem=original.stem)
    relative = safe_relative_posix_path(
        relative_metadata, f"{record['id']}.metadata_path"
    )
    metadata_path = source_root.joinpath(*relative.parts)
    try:
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CorpusError(
            f"{record['id']}: cannot read metadata {relative_metadata}: {error}"
        ) from error
    field = metadata_spec["license_field"]
    license_value = metadata.get(field)
    if not isinstance(license_value, str) or not license_value.strip():
        raise CorpusError(
            f"{record['id']}: metadata field {field!r} must be a non-empty string"
        )
    record["license"] = license_value
    record["metadata_path"] = relative_metadata
    record["metadata_sha256"] = sha256_file(metadata_path)


def extract_qmd_fences(path: Path, language: str) -> list[tuple[int, bytes]]:
    lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
    fences: list[tuple[int, bytes]] = []
    opening_pattern = re.compile(r"^(?P<fence>`{3,}|~{3,})(?P<info>.*)$")
    index = 0
    while index < len(lines):
        opening = opening_pattern.fullmatch(lines[index].strip())
        if opening is None:
            index += 1
            continue
        info = opening.group("info").strip()
        if not info:
            index += 1
            continue
        info_language = info.split(maxsplit=1)[0].lstrip("{").lstrip(".").rstrip("}")
        if info_language.casefold() != language.casefold():
            index += 1
            continue
        fence = opening.group("fence")
        closing_pattern = re.compile(rf"^{re.escape(fence[0])}{{{len(fence)},}}$")
        start = index + 2
        index += 1
        body: list[str] = []
        while index < len(lines) and closing_pattern.fullmatch(lines[index].strip()) is None:
            body.append(lines[index])
            index += 1
        if index >= len(lines):
            raise CorpusError(f"unterminated {language} fence in {path}:{start}")
        fences.append((start, "".join(body).encode("utf-8")))
        index += 1
    return fences


def discover_source(
    source: dict[str, Any],
    source_root: Path,
    destination_root: Path,
) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for discovery in source["discovery"]:
        files = matched_files(source_root, discovery["globs"])
        if discovery["type"] == "files":
            observed_count = len(files)
            for path in files:
                relative = path.relative_to(source_root).as_posix()
                record = copy_case(
                    source=source,
                    original=path,
                    relative_path=relative,
                    destination_root=destination_root,
                    discovery=discovery,
                )
                attach_case_metadata(record, source_root, path, discovery)
                records.append(record)
        else:
            observed_count = 0
            for path in files:
                relative = path.relative_to(source_root).as_posix()
                for fence_index, (line_start, content) in enumerate(
                    extract_qmd_fences(path, discovery["fence_language"]),
                    start=1,
                ):
                    observed_count += 1
                    records.append(
                        copy_case(
                            source=source,
                            original=path,
                            relative_path=relative,
                            destination_root=destination_root,
                            discovery=discovery,
                            case_suffix=f"#fence-{fence_index}",
                            line_start=line_start,
                            content=content,
                        )
                    )
        if observed_count != discovery["expected_count"]:
            raise CorpusError(
                f'{source["id"]}: {discovery["type"]} count drifted '
                f'(expected {discovery["expected_count"]}, got {observed_count})'
            )
    return records


def selected_manifests(arguments: argparse.Namespace) -> list[Path]:
    if arguments.manifest:
        return [path.resolve() for path in arguments.manifest]
    return list(DEFAULT_MANIFESTS)


def check_command(arguments: argparse.Namespace) -> int:
    manifests = [(path, load_manifest(path)) for path in selected_manifests(arguments)]
    source_ids: set[str] = set()
    for path, manifest in manifests:
        for source in manifest["sources"]:
            source_id = source["id"]
            if source_id in source_ids:
                raise CorpusError(f"duplicate source id across manifests: {source_id}")
            source_ids.add(source_id)
        print(f'{path.relative_to(REPO_ROOT)}: {len(manifest["sources"])} sources')
    print(f"diagnostic corpus manifests passed: {len(source_ids)} pinned sources")
    return 0


def materialize_command(arguments: argparse.Namespace) -> int:
    root = arguments.root.resolve()
    manifests = [(path, load_manifest(path)) for path in selected_manifests(arguments)]
    manifest_binding = [
        {
            "path": path.relative_to(REPO_ROOT).as_posix(),
            "sha256": sha256_file(path),
        }
        for path, _ in manifests
    ]
    materialized_root = root / "materialized" / "cases"
    shutil.rmtree(materialized_root, ignore_errors=True)
    materialized_root.mkdir(parents=True, exist_ok=True)

    records: list[dict[str, Any]] = []
    sources: list[dict[str, Any]] = []
    for _, manifest in manifests:
        for source in manifest["sources"]:
            archive = archive_path(root, source)
            download_archive(source, archive, arguments.offline)
            source_root = extract_archive(
                source,
                archive,
                root / "extracted" / source["id"],
            )
            source_records = discover_source(source, source_root, materialized_root)
            records.extend(source_records)
            sources.append(
                {
                    "id": source["id"],
                    "revision": source["revision"],
                    "archive_sha256": source["archive_sha256"],
                    "cases": len(source_records),
                }
            )
            print(f'{source["id"]}: materialized {len(source_records)} cases')

    records.sort(key=lambda record: record["id"])
    ids = [record["id"] for record in records]
    if len(ids) != len(set(ids)):
        raise CorpusError("materialized case IDs are not unique")
    index = {
        "schema_version": SCHEMA_VERSION,
        "manifest_binding": manifest_binding,
        "manifest_sha256": sha256_bytes(canonical_json(manifest_binding)),
        "sources": sorted(sources, key=lambda item: item["id"]),
        "cases": records,
        "counts": {
            "total": len(records),
            "stan": sum(record["language"] == "stan" for record in records),
            "jags": sum(record["language"] == "jags" for record in records),
        },
    }
    index_path = root / "materialized" / "index.json"
    index_path.parent.mkdir(parents=True, exist_ok=True)
    index_path.write_text(
        json.dumps(index, indent=2, ensure_ascii=False, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(f'wrote {len(records)} cases to {index_path}')
    return 0


def list_command(arguments: argparse.Namespace) -> int:
    root = arguments.root.resolve()
    index_path = root / "materialized" / "index.json"
    try:
        index = json.loads(index_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CorpusError(f"cannot read materialized index {index_path}: {error}") from error
    for source in index.get("sources", []):
        print(f'{source["id"]}: {source["cases"]} cases at {source["revision"]}')
    counts = index.get("counts", {})
    print(
        f'total={counts.get("total", 0)} '
        f'stan={counts.get("stan", 0)} jags={counts.get("jags", 0)}'
    )
    return 0


def clean_command(arguments: argparse.Namespace) -> int:
    root = arguments.root.resolve()
    for child in (root / "extracted", root / "materialized"):
        shutil.rmtree(child, ignore_errors=True)
    if arguments.downloads:
        shutil.rmtree(root / "downloads", ignore_errors=True)
    print(f"cleaned {root}")
    return 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    subparsers = result.add_subparsers(dest="command", required=True)

    check = subparsers.add_parser("check", help="validate committed manifests offline")
    check.add_argument("--manifest", action="append", type=Path)
    check.set_defaults(handler=check_command)

    materialize = subparsers.add_parser(
        "materialize", help="download, verify, and materialize every selected source"
    )
    materialize.add_argument("--manifest", action="append", type=Path)
    materialize.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    materialize.add_argument("--offline", action="store_true")
    materialize.add_argument(
        "--all",
        action="store_true",
        help="materialize both default manifests (the default when none are named)",
    )
    materialize.set_defaults(handler=materialize_command)

    listing = subparsers.add_parser("list", help="summarize a materialized index")
    listing.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    listing.set_defaults(handler=list_command)

    clean = subparsers.add_parser("clean", help="remove extracted/materialized corpus data")
    clean.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    clean.add_argument("--downloads", action="store_true")
    clean.set_defaults(handler=clean_command)
    return result


def main() -> int:
    arguments = parser().parse_args()
    try:
        return int(arguments.handler(arguments))
    except CorpusError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
