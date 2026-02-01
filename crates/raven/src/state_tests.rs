// Tests for workspace scanning functionality

#[cfg(test)]
mod workspace_scan_tests {
    use super::super::*;
    use std::fs;
    use tempfile::TempDir;
    use tower_lsp::lsp_types::Url;

    #[test]
    fn test_scan_workspace_finds_uppercase_r_files() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.R");
        fs::write(&test_file, "x <- 1").unwrap();

        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let (index, _, cross_file_entries, new_index_entries) = scan_workspace(&[workspace_url]);

        assert_eq!(index.len(), 1, "Should find 1 .R file");
        assert_eq!(cross_file_entries.len(), 1, "Should have 1 cross-file entry");
        assert_eq!(new_index_entries.len(), 1, "Should have 1 new index entry");
    }

    #[test]
    fn test_scan_workspace_finds_lowercase_r_files() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.r");
        fs::write(&test_file, "x <- 1").unwrap();

        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let (index, _, cross_file_entries, new_index_entries) = scan_workspace(&[workspace_url]);

        assert_eq!(index.len(), 1, "Should find 1 .r file");
        assert_eq!(cross_file_entries.len(), 1, "Should have 1 cross-file entry");
        assert_eq!(new_index_entries.len(), 1, "Should have 1 new index entry");
    }

    #[test]
    fn test_scan_workspace_finds_mixed_case_r_files() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create files with different case extensions
        fs::write(temp_dir.path().join("uppercase.R"), "x <- 1").unwrap();
        fs::write(temp_dir.path().join("lowercase.r"), "y <- 2").unwrap();
        
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let (index, _, cross_file_entries, new_index_entries) = scan_workspace(&[workspace_url]);

        assert_eq!(index.len(), 2, "Should find both .R and .r files");
        assert_eq!(cross_file_entries.len(), 2, "Should have 2 cross-file entries");
        assert_eq!(new_index_entries.len(), 2, "Should have 2 new index entries");
        
        // Verify both files are indexed
        let uris: Vec<String> = index.keys().map(|u| u.to_string()).collect();
        assert!(uris.iter().any(|u| u.contains("uppercase.R")), "Should find uppercase.R");
        assert!(uris.iter().any(|u| u.contains("lowercase.r")), "Should find lowercase.r");
    }

    #[test]
    fn test_scan_workspace_computes_cross_file_metadata() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create a file with a function definition
        let test_file = temp_dir.path().join("functions.r");
        fs::write(&test_file, "my_func <- function() { 42 }").unwrap();
        
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let (_, _, cross_file_entries, new_index_entries) = scan_workspace(&[workspace_url]);

        assert_eq!(cross_file_entries.len(), 1);
        assert_eq!(new_index_entries.len(), 1);
        
        // Verify the entry has artifacts with exported symbols
        let entry = cross_file_entries.values().next().unwrap();
        assert!(entry.artifacts.exported_interface.contains_key("my_func"), 
            "Should export my_func symbol");
        
        // Verify new index entry also has the same artifacts
        let new_entry = new_index_entries.values().next().unwrap();
        assert!(new_entry.artifacts.exported_interface.contains_key("my_func"), 
            "New index entry should export my_func symbol");
    }

    #[test]
    fn test_scan_workspace_recursive() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create nested directory structure
        let subdir = temp_dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        
        fs::write(temp_dir.path().join("root.r"), "x <- 1").unwrap();
        fs::write(subdir.join("nested.r"), "y <- 2").unwrap();
        
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let (index, _, _, new_index_entries) = scan_workspace(&[workspace_url]);

        assert_eq!(index.len(), 2, "Should find files in root and subdirectory");
        assert_eq!(new_index_entries.len(), 2, "Should have 2 new index entries");
    }

    #[test]
    fn test_scan_workspace_new_index_entry_has_all_fields() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create a file with library call and function definition
        let test_file = temp_dir.path().join("complete.r");
        fs::write(&test_file, r#"
library(dplyr)
my_func <- function(x) { x + 1 }
"#).unwrap();
        
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let (_, _, _, new_index_entries) = scan_workspace(&[workspace_url]);

        assert_eq!(new_index_entries.len(), 1);
        
        let entry = new_index_entries.values().next().unwrap();
        
        // Verify all fields are populated
        assert!(!entry.contents.to_string().is_empty(), "Should have contents");
        assert!(entry.tree.is_some(), "Should have parsed tree");
        assert!(entry.loaded_packages.contains(&"dplyr".to_string()), "Should have loaded packages");
        assert!(entry.snapshot.size > 0, "Should have valid snapshot");
        assert!(entry.artifacts.exported_interface.contains_key("my_func"), 
            "Should have exported symbols in artifacts");
    }
}
