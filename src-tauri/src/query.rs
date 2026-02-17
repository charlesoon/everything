#[derive(Debug, PartialEq)]
pub enum SearchMode {
    Empty,
    NameSearch {
        name_like: String,
    },
    GlobName {
        name_like: String,
    },
    ExtSearch {
        ext: String,
        name_like: String,
    },
    PathSearch {
        path_like: String,
        name_like: String,
        dir_hint: String,
    },
}

pub fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

pub fn glob_to_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    for ch in pattern.chars() {
        match ch {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn parse_query(query: &str) -> SearchMode {
    let trimmed = query.trim();

    if trimmed.is_empty() {
        return SearchMode::Empty;
    }

    if trimmed.contains('/') {
        let last_slash = trimmed.rfind('/').unwrap();
        let dir_part = trimmed[..last_slash].trim();
        let name_part = trimmed[last_slash + 1..].trim();

        let path_like = if dir_part.is_empty() {
            "%".to_string()
        } else if has_glob_chars(dir_part) {
            format!("%{}/%", glob_to_like(dir_part))
        } else {
            format!("%{}/%", escape_like(dir_part))
        };

        let name_like = if name_part.is_empty() {
            "%".to_string()
        } else if has_glob_chars(name_part) {
            glob_to_like(name_part)
        } else {
            format!("%{}%", escape_like(name_part))
        };

        return SearchMode::PathSearch {
            path_like,
            name_like,
            dir_hint: dir_part.to_string(),
        };
    }

    if let Some(ext_part) = trimmed.strip_prefix("*.") {
        if !ext_part.is_empty() && !ext_part.contains('/') && !has_glob_chars(ext_part) {
            return SearchMode::ExtSearch {
                ext: ext_part.to_lowercase(),
                name_like: glob_to_like(trimmed),
            };
        }
    }

    if has_glob_chars(trimmed) {
        return SearchMode::GlobName {
            name_like: glob_to_like(trimmed),
        };
    }

    SearchMode::NameSearch {
        name_like: format!("%{}%", escape_like(trimmed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query() {
        assert_eq!(parse_query(""), SearchMode::Empty);
        assert_eq!(parse_query("   "), SearchMode::Empty);
    }

    #[test]
    fn name_query() {
        match parse_query("hello") {
            SearchMode::NameSearch { name_like } => assert_eq!(name_like, "%hello%"),
            other => panic!("expected NameSearch, got {:?}", other),
        }
    }

    #[test]
    fn glob_star() {
        assert_eq!(
            parse_query("*.md"),
            SearchMode::ExtSearch {
                ext: "md".to_string(),
                name_like: "%.md".to_string(),
            }
        );
    }

    #[test]
    fn glob_question() {
        assert_eq!(
            parse_query("spec?.md"),
            SearchMode::GlobName {
                name_like: "spec_.md".to_string()
            }
        );
    }

    #[test]
    fn path_search_dir_only() {
        match parse_query("desktop/") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%desktop/%");
                assert_eq!(name_like, "%");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn path_search_dir_and_name() {
        match parse_query("desktop/*.png") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%desktop/%");
                assert_eq!(name_like, "%.png");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn path_search_root_slash() {
        match parse_query("/*.txt") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%");
                assert_eq!(name_like, "%.txt");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn path_search_plain_name() {
        match parse_query("src/main") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%src/%");
                assert_eq!(name_like, "%main%");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn glob_to_like_escapes() {
        assert_eq!(glob_to_like("100%_done"), "100\\%\\_done");
        assert_eq!(glob_to_like("a\\b"), "a\\\\b");
    }

    #[test]
    fn escape_like_works() {
        assert_eq!(escape_like("a%b_c\\d"), "a\\%b\\_c\\\\d");
    }

    #[test]
    fn special_chars_name_search() {
        assert_eq!(
            parse_query("@#$"),
            SearchMode::NameSearch {
                name_like: "%@#$%".to_string()
            }
        );
    }

    #[test]
    fn dir_glob_pattern() {
        match parse_query("*desktop/*.png") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%%desktop/%");
                assert_eq!(name_like, "%.png");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn path_search_trims_spaces_after_slash() {
        match parse_query("c_desktop/ *.png") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%c\\_desktop/%");
                assert_eq!(name_like, "%.png");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn path_search_trims_spaces_around_parts() {
        match parse_query("c_desktop / *.png") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%c\\_desktop/%");
                assert_eq!(name_like, "%.png");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn scenario_name_search() {
        match parse_query("a_desktop") {
            SearchMode::NameSearch { name_like } => {
                assert_eq!(name_like, "%a\\_desktop%");
            }
            other => panic!("expected NameSearch, got {:?}", other),
        }
    }

    #[test]
    fn scenario_subdir_search() {
        match parse_query("a_desktop/ subdirectory") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%a\\_desktop/%");
                assert_eq!(name_like, "%subdirectory%");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn scenario_glob_under_dir() {
        match parse_query("a_desktop/ *.png") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%a\\_desktop/%");
                assert_eq!(name_like, "%.png");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn scenario_prefix_glob_under_dir() {
        match parse_query("a_desktop/ test*.png") {
            SearchMode::PathSearch {
                path_like,
                name_like,
                ..
            } => {
                assert_eq!(path_like, "%a\\_desktop/%");
                assert_eq!(name_like, "test%.png");
            }
            other => panic!("expected PathSearch, got {:?}", other),
        }
    }

    #[test]
    fn ext_search_simple() {
        match parse_query("*.png") {
            SearchMode::ExtSearch { ext, name_like } => {
                assert_eq!(ext, "png");
                assert_eq!(name_like, "%.png");
            }
            other => panic!("expected ExtSearch, got {:?}", other),
        }
    }

    #[test]
    fn ext_search_uppercase() {
        match parse_query("*.PDF") {
            SearchMode::ExtSearch { ext, name_like } => {
                assert_eq!(ext, "pdf");
                assert_eq!(name_like, "%.PDF");
            }
            other => panic!("expected ExtSearch, got {:?}", other),
        }
    }

    #[test]
    fn ext_search_not_for_complex_glob() {
        // *.t?t should remain GlobName, not ExtSearch
        assert!(matches!(parse_query("*.t?t"), SearchMode::GlobName { .. }));
    }

    #[test]
    fn ext_search_not_for_path() {
        // dir/*.png should remain PathSearch, not ExtSearch
        assert!(matches!(
            parse_query("dir/*.png"),
            SearchMode::PathSearch { .. }
        ));
    }
}
