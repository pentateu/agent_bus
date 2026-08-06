use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// A concrete, wildcard-free topic such as `iot_base/dev_01`.
///
/// The first segment is the partition, which is the isolation boundary between
/// unrelated projects sharing one machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Topic {
    raw: String,
    /// Byte length of the first segment, so `partition()` is a cheap slice.
    partition_len: usize,
}

impl Topic {
    /// Parse a concrete topic. Wildcards are rejected — use [`Pattern`] for those.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidTopic`] if the topic is empty, has empty
    /// segments, contains whitespace, or contains a wildcard.
    pub fn parse(input: &str) -> Result<Self, CoreError> {
        let err = |reason| CoreError::InvalidTopic { input: input.to_owned(), reason };

        for segment in input.split('/') {
            if segment.is_empty() {
                // Also covers wholly empty input: `"".split('/')` yields one
                // empty segment, so there is no separate empty-input branch.
                return Err(err("segments must not be empty"));
            }
            if segment.contains('*') {
                return Err(err("wildcards are not allowed in a concrete topic"));
            }
            if segment.chars().any(char::is_whitespace) {
                return Err(err("segments must not contain whitespace"));
            }
        }

        // `split` always yields at least one segment, and the loop above proved
        // the first one is non-empty, so this cannot panic.
        let partition_len = input.split('/').next().unwrap_or_default().len();

        Ok(Self { raw: input.to_owned(), partition_len })
    }

    /// The isolation boundary: the first segment.
    #[must_use]
    pub fn partition(&self) -> &str {
        &self.raw[..self.partition_len]
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    fn segments(&self) -> std::str::Split<'_, char> {
        self.raw.split('/')
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl TryFrom<String> for Topic {
    type Error = CoreError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Topic> for String {
    fn from(value: Topic) -> Self {
        value.raw
    }
}

/// One element of a parsed subscription pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    /// Match this exact segment.
    Literal(String),
    /// `*` — match exactly one segment, any value.
    One,
    /// `**` — match one or more segments. Only ever the final element.
    Rest,
}

/// A subscription pattern such as `iot_base/*`, `iot_base/**`, or `iot_base`.
///
/// Grammar:
/// - `*` matches exactly one segment.
/// - `**` matches one or more segments and must be the final element.
/// - A bare partition name is shorthand for "the partition and everything in it".
///
/// The first segment must be a literal: a wildcard there would let a
/// subscription escape its partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    raw: String,
    partition: String,
    segments: Vec<Seg>,
    /// True when built from a bare partition name, which also matches the
    /// partition topic itself (not just its children).
    bare_partition: bool,
}

impl Pattern {
    /// Parse a subscription pattern.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidPattern`] for empty input, empty segments,
    /// whitespace, a wildcarded first segment, `**` in a non-final position, or
    /// a segment that mixes `*` with other characters (e.g. `dev_*`).
    pub fn parse(input: &str) -> Result<Self, CoreError> {
        let err = |reason| CoreError::InvalidPattern { input: input.to_owned(), reason };

        let raw_segments: Vec<&str> = input.split('/').collect();
        for segment in &raw_segments {
            if segment.is_empty() {
                // Also covers wholly empty input, as in `Topic::parse`.
                return Err(err("segments must not be empty"));
            }
            if segment.chars().any(char::is_whitespace) {
                return Err(err("segments must not contain whitespace"));
            }
            if segment.contains('*') && *segment != "*" && *segment != "**" {
                return Err(err("partial wildcards like `dev_*` are not supported"));
            }
        }

        // `split` always yields at least one segment, and the loop above proved
        // the first one is non-empty, so this cannot panic.
        let first = raw_segments.first().copied().unwrap_or_default();
        if first == "*" || first == "**" {
            return Err(err("the partition segment must be a literal name"));
        }
        let partition = first.to_owned();

        let mut segments = Vec::with_capacity(raw_segments.len() + 1);
        let last_index = raw_segments.len() - 1;
        for (i, segment) in raw_segments.iter().enumerate() {
            let seg = match *segment {
                "*" => Seg::One,
                "**" => {
                    if i != last_index {
                        return Err(err("`**` must be the final segment"));
                    }
                    Seg::Rest
                }
                literal => Seg::Literal(literal.to_owned()),
            };
            segments.push(seg);
        }

        // Bare partition name: shorthand for `<partition>/**`, but it also
        // matches the partition topic itself.
        let bare_partition = raw_segments.len() == 1;
        if bare_partition {
            segments.push(Seg::Rest);
        }

        Ok(Self { raw: input.to_owned(), partition, segments, bare_partition })
    }

    /// The partition this pattern is confined to.
    #[must_use]
    pub fn partition(&self) -> &str {
        &self.partition
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Does this pattern select `topic`?
    ///
    /// Cross-partition matches are impossible by construction: the first
    /// segment is always a literal and is compared first.
    #[must_use]
    pub fn matches(&self, topic: &Topic) -> bool {
        if topic.partition() != self.partition {
            return false;
        }

        let topic_segments: Vec<&str> = topic.segments().collect();

        // A bare partition pattern also matches the partition topic itself.
        if self.bare_partition && topic_segments.len() == 1 {
            return true;
        }

        let mut t = 0usize;
        for (i, seg) in self.segments.iter().enumerate() {
            match seg {
                Seg::Rest => {
                    // `**` is always final and requires at least one segment.
                    debug_assert_eq!(i, self.segments.len() - 1);
                    return topic_segments.len() > t;
                }
                Seg::One => {
                    if t >= topic_segments.len() {
                        return false;
                    }
                    t += 1;
                }
                Seg::Literal(expected) => {
                    if topic_segments.get(t) != Some(&expected.as_str()) {
                        return false;
                    }
                    t += 1;
                }
            }
        }

        t == topic_segments.len()
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_parses_and_exposes_partition() {
        let t = Topic::parse("iot_base/dev_01").unwrap();
        assert_eq!(t.partition(), "iot_base");
        assert_eq!(t.as_str(), "iot_base/dev_01");
    }

    #[test]
    fn single_segment_topic_is_its_own_partition() {
        let t = Topic::parse("iot_base").unwrap();
        assert_eq!(t.partition(), "iot_base");
    }

    #[test]
    fn topic_rejects_empty_and_malformed() {
        assert!(Topic::parse("").is_err());
        assert!(Topic::parse("/leading").is_err());
        assert!(Topic::parse("trailing/").is_err());
        assert!(Topic::parse("double//slash").is_err());
        assert!(Topic::parse("has space/x").is_err());
    }

    #[test]
    fn topic_rejects_wildcards() {
        assert!(Topic::parse("iot_base/*").is_err());
        assert!(Topic::parse("iot_base/**").is_err());
    }

    #[test]
    fn star_matches_exactly_one_segment() {
        let p = Pattern::parse("iot_base/*").unwrap();
        assert!(p.matches(&Topic::parse("iot_base/dev_01").unwrap()));
        assert!(!p.matches(&Topic::parse("iot_base/team/dev_01").unwrap()));
        assert!(!p.matches(&Topic::parse("iot_base").unwrap()));
    }

    #[test]
    fn doublestar_matches_one_or_more_segments() {
        let p = Pattern::parse("iot_base/**").unwrap();
        assert!(p.matches(&Topic::parse("iot_base/dev_01").unwrap()));
        assert!(p.matches(&Topic::parse("iot_base/team/dev_01").unwrap()));
        assert!(p.matches(&Topic::parse("iot_base/a/b/c").unwrap()));
        // ** requires at least one segment, so the bare partition does not match
        assert!(!p.matches(&Topic::parse("iot_base").unwrap()));
    }

    #[test]
    fn bare_partition_is_shorthand_for_everything_below() {
        let p = Pattern::parse("iot_base").unwrap();
        assert!(p.matches(&Topic::parse("iot_base/dev_01").unwrap()));
        assert!(p.matches(&Topic::parse("iot_base/a/b").unwrap()));
        // shorthand also matches the partition topic itself
        assert!(p.matches(&Topic::parse("iot_base").unwrap()));
    }

    #[test]
    fn pattern_never_matches_across_partitions() {
        let p = Pattern::parse("iot_base/**").unwrap();
        assert!(!p.matches(&Topic::parse("other/dev_01").unwrap()));
        let bare = Pattern::parse("iot_base").unwrap();
        assert!(!bare.matches(&Topic::parse("other/x").unwrap()));
    }

    #[test]
    fn pattern_exposes_its_partition() {
        assert_eq!(Pattern::parse("iot_base/**").unwrap().partition(), "iot_base");
        assert_eq!(Pattern::parse("iot_base").unwrap().partition(), "iot_base");
        assert_eq!(Pattern::parse("iot_base/*").unwrap().partition(), "iot_base");
    }

    #[test]
    fn partition_segment_may_not_be_a_wildcard() {
        // A wildcard in the first segment would break partition isolation.
        assert!(Pattern::parse("*/dev_01").is_err());
        assert!(Pattern::parse("**").is_err());
        assert!(Pattern::parse("*").is_err());
    }

    #[test]
    fn literal_pattern_matches_only_itself() {
        let p = Pattern::parse("iot_base/dev_01").unwrap();
        assert!(p.matches(&Topic::parse("iot_base/dev_01").unwrap()));
        assert!(!p.matches(&Topic::parse("iot_base/dev_02").unwrap()));
    }
}
