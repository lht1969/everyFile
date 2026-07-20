use chrono::{DateTime, Local};
use glob::Pattern;
use regex::Regex;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub keywords: Vec<String>,
    /// 预小写化的 keywords，避免在 matches_entry 中为每个文件重复 to_lowercase
    /// 在 parse() 时一次性计算，221万文件搜索时节省 221万 × k 次字符串分配
    keywords_lower: Vec<String>,
    pub glob_patterns: Vec<Pattern>,
    pub size_filter: Option<SizeFilter>,
    pub date_filter: Option<DateFilter>,
    pub path_filter: Option<String>,
    /// 预小写化的 path_filter，避免在 matches_entry 中为每个文件重复 to_lowercase
    path_filter_lower: Option<String>,
    pub path_filter_dir_only: bool,
    pub regex_pattern: Option<Regex>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SizeFilter {
    pub operator: SizeOperator,
    pub value: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SizeOperator {
    GreaterThan,
    LessThan,
    Equal,
    GreaterOrEqual,
    LessOrEqual,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DateFilter {
    pub date_type: DateType,
    pub operator: DateOperator,
    pub date: Option<DateTime<Local>>,
    pub start: Option<DateTime<Local>>,
    pub end: Option<DateTime<Local>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DateOperator {
    Equal,
    GreaterThan,
    LessThan,
    GreaterOrEqual,
    LessOrEqual,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DateType {
    Created,
    Modified,
    Accessed,
}

impl SearchQuery {
    fn contains_glob_chars(s: &str) -> bool {
        s.contains('*') || s.contains('?') || s.contains('[') || s.contains(']')
    }

    pub fn parse(query_str: &str) -> Self {
        let mut keywords = Vec::new();
        let mut glob_patterns = Vec::new();
        let mut size_filter = None;
        let mut date_filter = None;
        let mut path_filter = None;
        let mut path_filter_dir_only = false;
        let mut regex_pattern = None;

        let parts: Vec<&str> = query_str.split_whitespace().collect();
        let mut i = 0;
        while i < parts.len() {
            let part = parts[i];
            if part.starts_with("size:") {
                size_filter = Self::parse_size_filter(part);
            } else if part.starts_with("datemodified:")
                || part.starts_with("datecreated:")
                || part.starts_with("dateaccessed:")
                || part.starts_with("dm:")
                || part.starts_with("dc:")
                || part.starts_with("da:")
            {
                date_filter = Self::parse_date_filter(part);
            } else if let Some(path_part) = part.strip_prefix("path:") {
                path_filter = Some(path_part.to_string());
                if i + 1 < parts.len() {
                    let next = parts[i + 1];
                    if next == ":folders" || next == ":folder" {
                        path_filter_dir_only = true;
                        i += 1;
                    }
                }
            } else if let Some(regex_part) = part.strip_prefix("regex:") {
                if let Ok(re) = Regex::new(regex_part) {
                    regex_pattern = Some(re);
                }
            } else if part == ":folders" || part == ":folder" {
                path_filter_dir_only = true;
            } else if Self::contains_glob_chars(part) {
                if let Ok(pattern) = Pattern::new(part) {
                    glob_patterns.push(pattern);
                } else {
                    keywords.push(part.to_string());
                }
            } else {
                keywords.push(part.to_string());
            }
            i += 1;
        }

        // 预小写化 keywords 和 path_filter，避免在 matches_entry 中为每个文件重复计算
        // 对于 221万文件的搜索，这 saves 221万 × (k+1) 次字符串分配
        let keywords_lower = keywords.iter().map(|k| k.to_lowercase()).collect();
        let path_filter_lower = path_filter.as_ref().map(|p| p.to_lowercase());

        Self {
            keywords,
            keywords_lower,
            glob_patterns,
            size_filter,
            date_filter,
            path_filter,
            path_filter_lower,
            path_filter_dir_only,
            regex_pattern,
        }
    }

    #[allow(dead_code)]
    pub fn matches(&self, file: &crate::search::SearchResult) -> bool {
        if !self.keywords.is_empty() {
            let name_lower = file.name.to_lowercase();
            if !self
                .keywords
                .iter()
                .all(|kw| name_lower.contains(&kw.to_lowercase()))
            {
                return false;
            }
        }
        if !self.glob_patterns.is_empty()
            && !self
                .glob_patterns
                .iter()
                .all(|p| p.matches_path(std::path::Path::new(file.name.as_str())))
        {
            return false;
        }
        if let Some(ref size_filter) = self.size_filter {
            if !size_filter.matches(file.size) {
                return false;
            }
        }
        if let Some(ref date_filter) = self.date_filter {
            if let Some(ref target_date) = date_filter.date {
                let file_ts = file.modified_time;
                let target_ts = target_date.timestamp();
                let target_end_ts = target_date.timestamp() + 86399;
                let matches = match date_filter.operator {
                    DateOperator::Equal => file_ts >= target_ts && file_ts <= target_end_ts,
                    DateOperator::GreaterThan => file_ts > target_end_ts,
                    DateOperator::LessThan => file_ts < target_ts,
                    DateOperator::GreaterOrEqual => file_ts >= target_ts,
                    DateOperator::LessOrEqual => file_ts <= target_end_ts,
                };
                if !matches {
                    return false;
                }
            }
        }
        if let Some(ref path_filter) = self.path_filter {
            if !file
                .path
                .to_lowercase()
                .contains(&path_filter.to_lowercase())
            {
                return false;
            }
        }
        if self.path_filter_dir_only && !file.is_directory {
            return false;
        }
        if let Some(ref regex_pattern) = self.regex_pattern {
            if !regex_pattern.is_match(&file.name) {
                return false;
            }
        }
        true
    }

    /// 针对 FileEntry 的匹配函数
    ///
    /// 与 matches() 的区别：
    /// - FileEntry 没有 path 字段，需通过 full_path 参数传入解析后的路径
    /// - 当查询不含 path_filter 时，full_path 可传空字符串以跳过路径检查
    /// - modified_time 从 i32 提升为 i64 用于比较
    ///
    /// 性能优化：使用预小写化的 keywords_lower 和 path_filter_lower，
    /// 避免在 221万文件循环中为每个文件重复 to_lowercase 关键词
    pub fn matches_entry(&self, entry: &crate::search::FileEntry, full_path: &str) -> bool {
        if !self.keywords_lower.is_empty() {
            // 注意：name 仍需 per-file to_lowercase，但 keywords 已预计算
            // 使用 to_ascii_lowercase 比 to_lowercase 更快（仅处理 ASCII，跳过 Unicode case folding）
            // 对于中文文件名无影响（中文无大小写区分）
            let name_lower = entry.name.to_ascii_lowercase();
            if !self.keywords_lower.iter().all(|kw| name_lower.contains(kw)) {
                return false;
            }
        }
        if !self.glob_patterns.is_empty()
            && !self
                .glob_patterns
                .iter()
                .all(|p| p.matches_path(std::path::Path::new(entry.name.as_str())))
        {
            return false;
        }
        if let Some(ref size_filter) = self.size_filter {
            if !size_filter.matches(entry.size) {
                return false;
            }
        }
        if let Some(ref date_filter) = self.date_filter {
            if let Some(ref target_date) = date_filter.date {
                let file_ts = entry.modified_time as i64;
                let target_ts = target_date.timestamp();
                let target_end_ts = target_date.timestamp() + 86399;
                let matches = match date_filter.operator {
                    DateOperator::Equal => file_ts >= target_ts && file_ts <= target_end_ts,
                    DateOperator::GreaterThan => file_ts > target_end_ts,
                    DateOperator::LessThan => file_ts < target_ts,
                    DateOperator::GreaterOrEqual => file_ts >= target_ts,
                    DateOperator::LessOrEqual => file_ts <= target_end_ts,
                };
                if !matches {
                    return false;
                }
            }
        }
        // 使用预小写化的 path_filter_lower，避免 per-file to_lowercase
        if let Some(ref path_filter_lower) = self.path_filter_lower {
            if !full_path.to_ascii_lowercase().contains(path_filter_lower) {
                return false;
            }
        }
        if self.path_filter_dir_only && !entry.is_directory {
            return false;
        }
        if let Some(ref regex_pattern) = self.regex_pattern {
            if !regex_pattern.is_match(&entry.name) {
                return false;
            }
        }
        true
    }

    fn parse_size_filter(part: &str) -> Option<SizeFilter> {
        let value_str = part[5..].trim();

        let (operator_str, value_str) = if let Some(rest) = value_str.strip_prefix(">=") {
            (">=", rest)
        } else if let Some(rest) = value_str.strip_prefix("<=") {
            ("<=", rest)
        } else if let Some(rest) = value_str.strip_prefix(">") {
            (">", rest)
        } else if let Some(rest) = value_str.strip_prefix("<") {
            ("<", rest)
        } else if let Some(rest) = value_str.strip_prefix("=") {
            ("=", rest)
        } else {
            (">=", value_str)
        };

        let value = Self::parse_size_value(value_str.trim())?;

        let operator = match operator_str {
            ">" => SizeOperator::GreaterThan,
            "<" => SizeOperator::LessThan,
            "=" => SizeOperator::Equal,
            ">=" => SizeOperator::GreaterOrEqual,
            "<=" => SizeOperator::LessOrEqual,
            _ => SizeOperator::GreaterOrEqual,
        };

        Some(SizeFilter { operator, value })
    }

    fn parse_size_value(value_str: &str) -> Option<u64> {
        let value_str = value_str.to_uppercase();

        if value_str.ends_with("GB") {
            value_str[..value_str.len() - 2]
                .parse::<u64>()
                .ok()
                .map(|v| v * 1024 * 1024 * 1024)
        } else if value_str.ends_with("MB") {
            value_str[..value_str.len() - 2]
                .parse::<u64>()
                .ok()
                .map(|v| v * 1024 * 1024)
        } else if value_str.ends_with("KB") {
            value_str[..value_str.len() - 2]
                .parse::<u64>()
                .ok()
                .map(|v| v * 1024)
        } else if value_str.ends_with("B") {
            value_str[..value_str.len() - 1].parse::<u64>().ok()
        } else {
            value_str.parse::<u64>().ok()
        }
    }

    fn parse_date_filter(part: &str) -> Option<DateFilter> {
        let (date_type, after_prefix) = if let Some(rest) = part.strip_prefix("datemodified:") {
            (DateType::Modified, rest)
        } else if let Some(rest) = part.strip_prefix("dm:") {
            (DateType::Modified, rest)
        } else if let Some(rest) = part.strip_prefix("datecreated:") {
            (DateType::Created, rest)
        } else if let Some(rest) = part.strip_prefix("dc:") {
            (DateType::Created, rest)
        } else if let Some(rest) = part.strip_prefix("dateaccessed:") {
            (DateType::Accessed, rest)
        } else if let Some(rest) = part.strip_prefix("da:") {
            (DateType::Accessed, rest)
        } else {
            return None;
        };

        let date_str = after_prefix.trim();

        let (operator_str, date_value_str) = if let Some(rest) = date_str.strip_prefix(">=") {
            (">=", rest)
        } else if let Some(rest) = date_str.strip_prefix("<=") {
            ("<=", rest)
        } else if let Some(rest) = date_str.strip_prefix(">") {
            (">", rest)
        } else if let Some(rest) = date_str.strip_prefix("<") {
            ("<", rest)
        } else if let Some(rest) = date_str.strip_prefix("=") {
            ("=", rest)
        } else {
            ("=", date_str)
        };

        let operator = match operator_str {
            ">" => DateOperator::GreaterThan,
            "<" => DateOperator::LessThan,
            "=" => DateOperator::Equal,
            ">=" => DateOperator::GreaterOrEqual,
            "<=" => DateOperator::LessOrEqual,
            _ => DateOperator::GreaterOrEqual,
        };

        let parsed_date = match date_value_str {
            "today" => Some(Local::now().date_naive()),
            "yesterday" => Some(Local::now().date_naive().pred_opt()?),
            _ => {
                let formats = ["%Y%m%d", "%Y-%m-%d", "%Y/%m/%d"];
                let mut result = None;
                for fmt in &formats {
                    if let Ok(d) = chrono::NaiveDate::parse_from_str(date_value_str, fmt) {
                        result = Some(d);
                        break;
                    }
                }
                result
            }
        };

        if let Some(date) = parsed_date {
            let start = date
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .with_timezone(&Local);
            let end = date
                .and_hms_opt(23, 59, 59)
                .unwrap()
                .and_utc()
                .with_timezone(&Local);

            Some(DateFilter {
                date_type,
                operator,
                date: Some(start),
                start: Some(start),
                end: Some(end),
            })
        } else {
            Some(DateFilter {
                date_type,
                operator,
                date: None,
                start: None,
                end: None,
            })
        }
    }
}

impl SizeFilter {
    pub fn matches(&self, size: u64) -> bool {
        match self.operator {
            SizeOperator::GreaterThan => size > self.value,
            SizeOperator::LessThan => size < self.value,
            SizeOperator::Equal => size == self.value,
            SizeOperator::GreaterOrEqual => size >= self.value,
            SizeOperator::LessOrEqual => size <= self.value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_folder_modifier_standalone() {
        let q = SearchQuery::parse(":folder");
        assert!(q.path_filter_dir_only);
        assert!(q.keywords.is_empty());

        let q = SearchQuery::parse(":folders");
        assert!(q.path_filter_dir_only);
        assert!(q.keywords.is_empty());
    }

    #[test]
    fn test_folder_modifier_after_path() {
        let q = SearchQuery::parse("path:C:\\Users :folder");
        assert!(q.path_filter_dir_only);
        assert_eq!(q.path_filter, Some("C:\\Users".into()));
        assert!(q.keywords.is_empty());
    }

    #[test]
    fn test_folder_modifier_before_path() {
        let q = SearchQuery::parse(":folder path:C:\\Users");
        assert!(q.path_filter_dir_only);
        assert_eq!(q.path_filter, Some("C:\\Users".into()));
        assert!(q.keywords.is_empty());
    }

    #[test]
    fn test_folder_modifier_with_keyword() {
        let q = SearchQuery::parse("local :folder path:C:\\Users");
        assert!(q.path_filter_dir_only);
        assert_eq!(q.path_filter, Some("C:\\Users".into()));
        assert_eq!(q.keywords, vec!["local"]);
    }

    #[test]
    fn test_bare_folders_is_keyword() {
        // bare "folders" should now be a plain keyword, not a modifier
        let q = SearchQuery::parse("folders");
        assert!(!q.path_filter_dir_only);
        assert_eq!(q.keywords, vec!["folders"]);
    }

    #[test]
    fn test_folder_modifier_matches_directory() {
        let q = SearchQuery::parse(":folder");
        assert!(q.matches(&crate::search::SearchResult {
            file_id: 1,
            name: "test".into(),
            path: "C:\\Users\\test".into(),
            size: 0,
            modified_time: 0,
            is_directory: true,
        }));
        assert!(!q.matches(&crate::search::SearchResult {
            file_id: 2,
            name: "file.txt".into(),
            path: "C:\\Users\\file.txt".into(),
            size: 100,
            modified_time: 0,
            is_directory: false,
        }));
    }

    #[test]
    fn test_keyword_name_only_not_path() {
        let q = SearchQuery::parse("local");
        assert!(!q.matches(&crate::search::SearchResult {
            file_id: 1,
            name: "EBWebView".into(),
            path: "C:\\Users\\lht\\AppData\\Local\\EBWebView".into(),
            size: 0,
            modified_time: 0,
            is_directory: true,
        }));
        assert!(q.matches(&crate::search::SearchResult {
            file_id: 2,
            name: "local".into(),
            path: "C:\\Somewhere".into(),
            size: 0,
            modified_time: 0,
            is_directory: true,
        }));
    }

    #[test]
    fn test_folder_modifier_matches_name_and_path_filter() {
        // keyword matches name, :folder ensures directory-only, path: filters by path
        let q = SearchQuery::parse("EBWebView :folder path:C:\\Users");
        assert!(q.matches(&crate::search::SearchResult {
            file_id: 1,
            name: "EBWebView".into(),
            path: "C:\\Users\\lht\\AppData\\Local\\EBWebView".into(),
            size: 0,
            modified_time: 0,
            is_directory: true,
        }));
        assert!(!q.matches(&crate::search::SearchResult {
            file_id: 2,
            name: "EBWebView".into(),
            path: "C:\\Users\\lht\\AppData\\Local\\EBWebView".into(),
            size: 0,
            modified_time: 0,
            is_directory: false,
        }));
    }
}
