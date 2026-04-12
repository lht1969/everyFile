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
    pub start: Option<DateTime<Local>>,
    pub end: Option<DateTime<Local>>,
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
                path_filter = Some(part[5..].to_string());
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
        let (date_type_str, date_str) = if part.starts_with("datemodified:") {
            (DateType::Modified, &part[13..])
        } else if part.starts_with("datecreated:") {
            (DateType::Created, &part[12..])
        } else if part.starts_with("dateaccessed:") {
            (DateType::Accessed, &part[13..])
        } else {
            return None;
        };

        let date_str = date_str.trim();

        let date = match date_str {
            "today" => Some(Local::now().date_naive()),
            "yesterday" => Some(Local::now().date_naive().pred_opt()?),
            "thisweek" => None,
            "thismonth" => None,
            "thisyear" => None,
            _ => chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                .ok()
                .map(|d| d),
        };

        Some(DateFilter {
            date_type: date_type_str,
            start: date.map(|d| {
                d.and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .with_timezone(&Local)
            }),
            end: date.map(|d| {
                d.and_hms_opt(23, 59, 59)
                    .unwrap()
                    .and_utc()
                    .with_timezone(&Local)
            }),
        })
    }

    pub fn matches(&self, name: &str) -> bool {
        if self.keywords.is_empty() {
            return true;
        }

        let name_lower = name.to_lowercase();

        for keyword in &self.keywords {
            let keyword_lower = keyword.to_lowercase();
            if !name_lower.contains(&keyword_lower) {
                return false;
            }
        }

        if let Some(ref pattern) = self.regex_pattern {
            return pattern.is_match(name);
        }

        true
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
