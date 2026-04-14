use crate::search::{SearchOptions, SearchResult};
use chrono::{DateTime, Local};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub keywords: Vec<String>,
    pub size_filter: Option<SizeFilter>,
    pub date_filter: Option<DateFilter>,
    pub path_filter: Option<String>,
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
    pub fn parse(query_str: &str) -> Self {
        let mut keywords = Vec::new();
        let mut size_filter = None;
        let mut date_filter = None;
        let mut path_filter = None;
        let mut path_filter_dir_only = false;
        let mut regex_pattern = None;

        let parts: Vec<&str> = query_str.split_whitespace().collect();

        for part in parts {
            if part.starts_with("size:") {
                size_filter = Self::parse_size_filter(part);
            } else if part.starts_with("datemodified:")
                || part.starts_with("datecreated:")
                || part.starts_with("dateaccessed:")
            {
                date_filter = Self::parse_date_filter(part);
            } else if part.starts_with("path:") {
                let path_part = &part[5..];
                if path_part.ends_with(" folders") || path_part.ends_with(" folder") {
                    path_filter_dir_only = true;
                    path_filter = Some(path_part[..path_part.len() - 8].trim_end().to_string());
                } else {
                    path_filter = Some(path_part.to_string());
                }
            } else if part.starts_with("regex:") {
                if let Ok(re) = Regex::new(&part[6..]) {
                    regex_pattern = Some(re);
                }
            } else {
                keywords.push(part.to_string());
            }
        }

        Self {
            keywords,
            size_filter,
            date_filter,
            path_filter,
            path_filter_dir_only,
            regex_pattern,
        }
    }

    fn parse_size_filter(part: &str) -> Option<SizeFilter> {
        let value_str = part[5..].trim();

        let (operator_str, value_str) = if value_str.starts_with(">") {
            (">", &value_str[1..])
        } else if value_str.starts_with("<") {
            ("<", &value_str[1..])
        } else if value_str.starts_with(">=") {
            (">=", &value_str[2..])
        } else if value_str.starts_with("<=") {
            ("<=", &value_str[2..])
        } else if value_str.starts_with("=") {
            ("=", &value_str[1..])
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
        let (date_type_str, after_prefix) = if part.starts_with("datemodified:") {
            (DateType::Modified, &part[13..])
        } else if part.starts_with("datecreated:") {
            (DateType::Created, &part[12..])
        } else if part.starts_with("dateaccessed:") {
            (DateType::Accessed, &part[13..])
        } else {
            return None;
        };

        let date_str = after_prefix.trim();

        let (operator_str, date_value_str) = if date_str.starts_with(">=") {
            (">=", &date_str[2..])
        } else if date_str.starts_with("<=") {
            ("<=", &date_str[2..])
        } else if date_str.starts_with(">") {
            (">", &date_str[1..])
        } else if date_str.starts_with("<") {
            ("<", &date_str[1..])
        } else if date_str.starts_with("=") {
            ("=", &date_str[1..])
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
            let start = date.and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .with_timezone(&Local);
            let end = date.and_hms_opt(23, 59, 59)
                .unwrap()
                .and_utc()
                .with_timezone(&Local);

            Some(DateFilter {
                date_type: date_type_str,
                operator,
                date: Some(start),
                start: Some(start),
                end: Some(end),
            })
        } else {
            Some(DateFilter {
                date_type: date_type_str,
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
