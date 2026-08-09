import { expect, test } from "bun:test";
import fs from "fs";
import path from "path";

import {
  getInitializationOptions,
  type RavenConfigurationInspection,
  type RavenWorkspaceConfiguration,
} from "../../editors/vscode/src/initializationOptions";

function createMockConfig(
  configuredSettings: Map<string, unknown>,
): RavenWorkspaceConfiguration {
  return {
    get<T>(key: string, defaultValue?: T): T | undefined {
      const value = configuredSettings.get(key) as T | undefined;
      return value !== undefined ? value : defaultValue;
    },
    inspect<T>(key: string): RavenConfigurationInspection<T> | undefined {
      const value = configuredSettings.get(key);
      if (value !== undefined) {
        return {
          globalValue: value as T,
        };
      }
      return undefined;
    },
  };
}

test("initialization options keep diagnostic defaults", () => {
  const options = getInitializationOptions(createMockConfig(new Map()));
  expect(options.diagnostics).toEqual({
    enabled: true,
    maxSyntaxDiagnosticsPerFile: 500,
  });
});

test("initialization options forward explicitly configured settings", () => {
  const options = getInitializationOptions(
    createMockConfig(
      new Map<string, unknown>([
        ["diagnostics.jags", "on"],
        ["diagnostics.stan", "off"],
        ["crossFile.backwardDependencies", "explicit"],
        ["crossFile.maxChainDepth", 42],
        ["completion.triggerOnOpenParen", false],
        ["packages.additionalLibraryPaths", ["/tmp/libA", "/tmp/libB"]],
        ["indentation.style", "rstudio-minus"],
      ]),
    ),
  );

  expect(options.diagnostics).toMatchObject({ jags: "on", stan: "off" });
  expect(options.crossFile).toMatchObject({
    backwardDependencies: "explicit",
    maxChainDepth: 42,
  });
  expect(options.completion).toEqual({ triggerOnOpenParen: false });
  expect(options.packages).toEqual({
    additionalLibraryPaths: ["/tmp/libA", "/tmp/libB"],
  });
  expect(options.indentation).toEqual({ style: "rstudio-minus" });
});

test("VS Code package metadata registers mixed-case JAGS and Stan extensions", () => {
  const packageJsonPath = path.join(
    import.meta.dir,
    "..",
    "..",
    "editors",
    "vscode",
    "package.json",
  );
  const pkg = JSON.parse(fs.readFileSync(packageJsonPath, "utf8"));
  const languages = pkg.contributes.languages as Array<{
    id: string;
    extensions: string[];
  }>;

  const jags = languages.find((language) => language.id === "jags");
  const stan = languages.find((language) => language.id === "stan");

  expect(jags?.extensions).toEqual([
    ".jags",
    ".Jags",
    ".JAGS",
    ".bugs",
    ".Bugs",
    ".BUGS",
    ".bug",
    ".buG",
    ".bUg",
    ".bUG",
    ".Bug",
    ".BuG",
    ".BUg",
    ".BUG",
  ]);
  expect(stan?.extensions).toEqual([".stan", ".Stan", ".STAN"]);
});

test("VS Code file watcher includes every contributed JAGS extension spelling", () => {
  const extensionSource = fs.readFileSync(
    path.join(import.meta.dir, "..", "..", "editors", "vscode", "src", "extension.ts"),
    "utf8",
  );
  const watcher = extensionSource.match(
    /createFileSystemWatcher\(\s*'\*\*\/\*\.\{([^']+)\}'/,
  );

  expect(watcher).not.toBeNull();
  const watchedExtensions = watcher![1].split(",");
  expect(watchedExtensions).toEqual([
    "r",
    "R",
    "rmd",
    "Rmd",
    "RMD",
    "rmarkdown",
    "Rmarkdown",
    "RMARKDOWN",
    "qmd",
    "Qmd",
    "QMD",
    "jags",
    "Jags",
    "JAGS",
    "bugs",
    "Bugs",
    "BUGS",
    "bug",
    "buG",
    "bUg",
    "bUG",
    "Bug",
    "BuG",
    "BUg",
    "BUG",
    "stan",
    "Stan",
    "STAN",
  ]);
  expect(watchedExtensions).not.toContain("bugx");
});

test("VS Code package metadata registers mixed-case R Markdown extensions", () => {
  const packageJsonPath = path.join(
    import.meta.dir,
    "..",
    "..",
    "editors",
    "vscode",
    "package.json",
  );
  const pkg = JSON.parse(fs.readFileSync(packageJsonPath, "utf8"));
  const languages = pkg.contributes.languages as Array<{
    id: string;
    extensions: string[];
  }>;

  const rmd = languages.find((language) => language.id === "rmd");

  expect(rmd?.extensions).toEqual([
    ".rmd",
    ".Rmd",
    ".RMD",
    ".rmarkdown",
    ".Rmarkdown",
    ".RMARKDOWN",
  ]);
});

test("VS Code package metadata associates .Rprofile with the r language", () => {
  // `.Rprofile` is R code with no `.R` extension, so VS Code only treats it as
  // R (live text-sync + highlighting + diagnostics) if the `r` language claims
  // it by filename. The server's live `.Rprofile` prelude refresh
  // (refresh_rprofile_prelude_from_buffer) depends on the editor sending
  // did_open/did_change/did_close for `.Rprofile` — which only happens when it
  // matches the LSP documentSelector's `{ language: 'r' }`.
  const packageJsonPath = path.join(
    import.meta.dir,
    "..",
    "..",
    "editors",
    "vscode",
    "package.json",
  );
  const pkg = JSON.parse(fs.readFileSync(packageJsonPath, "utf8"));
  const r = (
    pkg.contributes.languages as Array<{ id: string; filenames?: string[] }>
  ).find((language) => language.id === "r");

  expect(r?.filenames).toContain(".Rprofile");
});

test("VS Code package metadata activates on r/jags/stan via contributes.languages", () => {
  // VS Code >= 1.74 auto-generates onLanguage:* activation events from
  // contributes.languages, so Raven declares no explicit onLanguage entries
  // (they were dropped to clear manifest warnings). The language contribution
  // below is what now drives activation, so that is what we guard here. The
  // live runtime confirmation that these languages are actually registered
  // lives in the Mocha suite, editors/vscode/src/test/language-activation.test.ts.
  const packageJsonPath = path.join(
    import.meta.dir,
    "..",
    "..",
    "editors",
    "vscode",
    "package.json",
  );
  const pkg = JSON.parse(fs.readFileSync(packageJsonPath, "utf8"));
  const languageIds = (pkg.contributes.languages as Array<{ id: string }>).map(
    (language) => language.id,
  );

  expect(languageIds).toContain("r");
  expect(languageIds).toContain("jags");
  expect(languageIds).toContain("stan");
});

test("VS Code package metadata explicitly activates for Markdown CodeLens", () => {
  // Markdown is owned by VS Code, not contributed by Raven, so it does not
  // receive Raven's implicit activation event. The explicit event is required
  // for the programmatic CodeLens provider to register when a Markdown file is
  // the first Raven-relevant document opened in a window.
  const packageJsonPath = path.join(
    import.meta.dir,
    "..",
    "..",
    "editors",
    "vscode",
    "package.json",
  );
  const pkg = JSON.parse(fs.readFileSync(packageJsonPath, "utf8"));

  expect(pkg.activationEvents).toContain("onLanguage:markdown");
});

test("VS Code package metadata pins R Markdown files to the rmd language via files.associations", () => {
  // The Quarto extension contributes `editorLangId == quarto` for `.rmd` (it
  // accepts both `.qmd` and `.rmd`). When only Raven and Quarto are installed,
  // VS Code's `contributes.languages` resolver can pick Quarto's claim over
  // Raven's, leaving R Markdown files tagged as `quarto` — at which point Raven's
  // `raven.knit` keybinding (gated on `editorLangId == rmd || == r`) silently
  // stops firing on them. `contributes.languages` resolution order is not a
  // documented invariant, so we don't rely on outvoting Quarto there.
  //
  // REditorSupport.r-syntax used to mask the bug by contributing another `rmd`
  // claim, but Raven vendors its own grammars now, so users who uninstall
  // r-syntax can hit the latent ordering issue.
  //
  // `files.associations` is registered through VS Code's
  // `registerConfiguredLanguageAssociation` path, which takes precedence over
  // `contributes.languages` extension lookup. Shipping it as a
  // `configurationDefaults` entry fixes the resolution at the source and lets
  // users opt out with their own `files.associations`. Pin the contribution.
  const packageJsonPath = path.join(
    import.meta.dir,
    "..",
    "..",
    "editors",
    "vscode",
    "package.json",
  );
  const pkg = JSON.parse(fs.readFileSync(packageJsonPath, "utf8"));
  const associations = pkg.contributes.configurationDefaults?.[
    "files.associations"
  ] as Record<string, string> | undefined;

  expect(associations).toBeDefined();
  expect(associations).toMatchObject({
    "*.rmd": "rmd",
    "*.Rmd": "rmd",
    "*.RMD": "rmd",
    "*.rmarkdown": "rmd",
    "*.Rmarkdown": "rmd",
    "*.RMARKDOWN": "rmd",
  });

  // Never claim `.qmd` — Quarto owns that extension and its preview / render
  // commands live there.
  expect(associations).not.toHaveProperty("*.qmd");
  expect(associations).not.toHaveProperty("*.Qmd");
  expect(associations).not.toHaveProperty("*.QMD");
});

test("VS Code package metadata exposes send method setting", () => {
  const packageJsonPath = path.join(
    import.meta.dir,
    "..",
    "..",
    "editors",
    "vscode",
    "package.json",
  );
  const pkg = JSON.parse(fs.readFileSync(packageJsonPath, "utf8"));
  const setting =
    pkg.contributes.configuration.properties["raven.sendToR.sendMethod"];

  expect(setting).toMatchObject({
    type: "string",
    enum: ["auto", "paste", "tempfile"],
    default: "auto",
  });
  expect(setting.enumDescriptions).toHaveLength(3);
  expect(setting.description).toContain("Controls how Raven sends code to R");
});
