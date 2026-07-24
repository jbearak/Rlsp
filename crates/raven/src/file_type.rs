use tower_lsp::lsp_types::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileType {
    #[default]
    R,
    Jags,
    Stan,
}

pub fn file_type_from_uri(uri: &Url) -> FileType {
    let path = uri.path();
    let lower_path = path.to_ascii_lowercase();
    if lower_path.ends_with(".jags")
        || lower_path.ends_with(".bugs")
        || lower_path.ends_with(".bug")
    {
        FileType::Jags
    } else if lower_path.ends_with(".stan") {
        FileType::Stan
    } else {
        FileType::R
    }
}

pub fn file_type_from_language_id(language_id: &str) -> Option<FileType> {
    match language_id.to_ascii_lowercase().as_str() {
        "r" => Some(FileType::R),
        "jags" => Some(FileType::Jags),
        "stan" => Some(FileType::Stan),
        _ => None,
    }
}

pub fn file_type_from_language_id_or_uri(language_id: Option<&str>, uri: &Url) -> FileType {
    language_id
        .and_then(file_type_from_language_id)
        .unwrap_or_else(|| file_type_from_uri(uri))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_id_overrides_extensionless_uri() {
        let uri = Url::parse("untitled:Untitled-1").unwrap();
        assert_eq!(
            file_type_from_language_id_or_uri(Some("jags"), &uri),
            FileType::Jags
        );
        assert_eq!(
            file_type_from_language_id_or_uri(Some("stan"), &uri),
            FileType::Stan
        );
    }

    #[test]
    fn test_mixed_case_language_id() {
        assert_eq!(file_type_from_language_id("Stan"), Some(FileType::Stan));
        assert_eq!(file_type_from_language_id("JAGS"), Some(FileType::Jags));
        assert_eq!(file_type_from_language_id("R"), Some(FileType::R));
        assert_eq!(file_type_from_language_id("STAN"), Some(FileType::Stan));
        assert_eq!(file_type_from_language_id("Jags"), Some(FileType::Jags));
    }

    #[test]
    fn test_unknown_language_id_falls_back_to_uri() {
        let uri = Url::parse("file:///tmp/model.stan").unwrap();
        assert_eq!(
            file_type_from_language_id_or_uri(Some("plaintext"), &uri),
            FileType::Stan
        );
    }

    #[test]
    fn singular_bug_is_a_case_insensitive_jags_extension() {
        for extension in ["bug", "Bug", "BUG"] {
            let uri = Url::parse(&format!("file:///tmp/model.{extension}")).unwrap();
            assert_eq!(file_type_from_uri(&uri), FileType::Jags);
            assert_eq!(
                file_type_from_language_id_or_uri(Some("plaintext"), &uri),
                FileType::Jags
            );
        }

        let near_miss = Url::parse("file:///tmp/model.bugx").unwrap();
        assert_eq!(file_type_from_uri(&near_miss), FileType::R);
    }
}
