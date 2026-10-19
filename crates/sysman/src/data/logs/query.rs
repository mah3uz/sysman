use jiff::civil::Date;
use jiff::tz::TimeZone;
use journal::categories::Category;

/// A boot named in a query: `0` is this boot, `-1` the one before, or a 32-digit boot id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootRef {
    Back(u32),
    Id(String),
}

/// A parsed search string. Fields AND together; the values inside one field OR. Empty means "no restriction".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// `cat:` keys.
    pub categories: Vec<Category>,
    /// `prio:`: rows at this level or more severe (a number 0..=7 where lower is worse). Several values keep the
    /// least strict.
    pub max_priority: Option<u8>,
    /// `unit:`: matches a row's display source, so `ssh` and `ssh.service` are the same.
    pub units: Vec<String>,
    /// `source:`: the syslog identifier.
    pub sources: Vec<String>,
    /// `pid:`: matched while streaming messages, see [`Query::search`].
    pub pids: Vec<u32>,
    pub boots: Vec<BootRef>,
    /// `since:` and `until:` as microseconds since the epoch.
    pub since_us: Option<u64>,
    pub until_us: Option<u64>,
    /// Free text terms, all of which must be in the message (case-insensitive). A quoted phrase is one term.
    pub text: Vec<String>,
    /// Index into the finding list: only that finding's entries.
    pub finding: Option<usize>,
}

/// What the message-streaming scan has to check, since the index holds neither messages nor pids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSpec {
    /// Lower case.
    pub terms: Vec<String>,
    pub pids: Vec<u32>,
}

impl Query {
    /// Parses `unit:`, `prio:`, `boot:`, `source:`, `cat:`, `pid:`, `since:`, `until:` tokens (values may be
    /// quoted); every other word, a token with an unknown name or a value that does not parse included, is free
    /// text. `now_us` anchors `1h`-style times; `2026-10-15 20:00` is read in `tz`.
    pub fn parse(input: &str, now_us: u64, tz: &TimeZone) -> Query {
        let mut q = Query::default();
        for (word, quoted) in split(input) {
            if quoted || !q.token(&word, now_us, tz) {
                q.text.push(word);
            }
        }
        q
    }

    fn token(&mut self, word: &str, now_us: u64, tz: &TimeZone) -> bool {
        let Some((key, value)) = word.split_once(':') else {
            return false;
        };
        if value.is_empty() {
            return false;
        }
        match key.to_ascii_lowercase().as_str() {
            "unit" => self.units.push(value.to_string()),
            "source" => self.sources.push(value.to_string()),
            "cat" => match Category::from_key(&value.to_ascii_lowercase()) {
                Some(c) => self.categories.push(c),
                None => return false,
            },
            "prio" => match priority(value) {
                Some(p) => self.max_priority = Some(self.max_priority.map_or(p, |m| m.max(p))),
                None => return false,
            },
            "pid" => match value.parse() {
                Ok(p) => self.pids.push(p),
                Err(_) => return false,
            },
            "boot" => match boot(value) {
                Some(b) => self.boots.push(b),
                None => return false,
            },
            "since" => match time(value, now_us, tz) {
                Some(t) => self.since_us = Some(t),
                None => return false,
            },
            "until" => match time(value, now_us, tz) {
                Some(t) => self.until_us = Some(t),
                None => return false,
            },
            _ => return false,
        }
        true
    }

    /// True when nothing restricts the query.
    pub fn is_empty(&self) -> bool {
        *self == Query::default()
    }

    /// The part that needs the message-streaming scan: free text and pids. `None` when the index alone answers.
    pub fn search(&self) -> Option<SearchSpec> {
        if self.text.is_empty() && self.pids.is_empty() {
            return None;
        }
        Some(SearchSpec {
            terms: self.text.iter().map(|t| t.to_lowercase()).collect(),
            pids: self.pids.clone(),
        })
    }
}

fn split(input: &str) -> Vec<(String, bool)> {
    let mut out = vec![];
    let (mut cur, mut open, mut in_quote, mut starts_quoted) = (String::new(), false, false, false);
    for c in input.chars() {
        match c {
            '"' => {
                if !open {
                    open = true;
                    starts_quoted = true;
                }
                in_quote = !in_quote;
            }
            c if c.is_whitespace() && !in_quote => {
                if open {
                    out.push((std::mem::take(&mut cur), starts_quoted));
                    open = false;
                    starts_quoted = false;
                }
            }
            c => {
                open = true;
                cur.push(c);
            }
        }
    }
    if open {
        out.push((cur, starts_quoted));
    }
    out
}

fn priority(value: &str) -> Option<u8> {
    let level = match value.to_ascii_lowercase().as_str() {
        "emerg" | "emergency" => 0,
        "alert" => 1,
        "crit" | "critical" => 2,
        "err" | "error" => 3,
        "warn" | "warning" => 4,
        "notice" => 5,
        "info" => 6,
        "debug" => 7,
        other => other.parse::<u8>().ok().filter(|p| *p <= 7)?,
    };
    Some(level)
}

fn boot(value: &str) -> Option<BootRef> {
    if let Ok(n) = value.parse::<i64>() {
        return u32::try_from(-n).ok().map(BootRef::Back);
    }
    let id = value.replace('-', "").to_ascii_lowercase();
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(BootRef::Id(id))
}

fn time(value: &str, now_us: u64, tz: &TimeZone) -> Option<u64> {
    if let Some(ago) = relative(value) {
        return Some(now_us.saturating_sub(ago));
    }
    let (date, clock) = value.split_once([' ', 'T']).unwrap_or((value, "00:00"));
    let date: Date = date.parse().ok()?;
    let mut parts = clock.split(':').map(|p| p.parse::<i8>().ok());
    let (h, m) = (parts.next()??, parts.next()??);
    let s = parts.next().unwrap_or(Some(0))?;
    let us = date
        .at(h, m, s, 0)
        .to_zoned(tz.clone())
        .ok()?
        .timestamp()
        .as_microsecond();
    u64::try_from(us).ok()
}

fn relative(value: &str) -> Option<u64> {
    let unit = value.chars().last()?;
    let n: u64 = value[..value.len() - unit.len_utf8()].parse().ok()?;
    let secs = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        'w' => 7 * 86400,
        _ => return None,
    };
    n.checked_mul(secs * 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000_000;

    fn parse(s: &str) -> Query {
        Query::parse(s, NOW, &TimeZone::UTC)
    }

    #[test]
    fn tokens_and_free_text_separate() {
        let q = parse("unit:sshd.service prio:warning disk error");
        assert_eq!(q.units, ["sshd.service"]);
        assert_eq!(q.max_priority, Some(4));
        assert_eq!(q.text, ["disk", "error"]);
    }

    #[test]
    fn repeated_values_collect_so_they_can_or() {
        let q = parse("cat:kernel cat:power source:a source:b pid:1 pid:2");
        assert_eq!(q.categories, [Category::Kernel, Category::BootPower]);
        assert_eq!(q.sources, ["a", "b"]);
        assert_eq!(q.pids, [1, 2]);
    }

    #[test]
    fn a_priority_is_that_level_and_more_severe_and_repeats_keep_the_loosest() {
        assert_eq!(parse("prio:err").max_priority, Some(3));
        assert_eq!(parse("prio:3").max_priority, Some(3));
        assert_eq!(parse("prio:err prio:notice").max_priority, Some(5));
        assert_eq!(parse("prio:9").max_priority, None, "out of range is text");
        assert_eq!(parse("prio:9").text, ["prio:9"]);
    }

    #[test]
    fn unknown_tokens_and_bad_values_stay_free_text() {
        let q = parse("foo:bar cat:nonsense since:soon pid:x boot:1 unit:");
        assert!(q.categories.is_empty() && q.since_us.is_none() && q.pids.is_empty() && q.boots.is_empty());
        assert_eq!(
            q.text,
            ["foo:bar", "cat:nonsense", "since:soon", "pid:x", "boot:1", "unit:"]
        );
    }

    #[test]
    fn quotes_keep_spaces_and_a_quoted_word_is_never_a_token() {
        let q = parse(r#"unit:"my app.service" "unit:x y" "out of memory""#);
        assert_eq!(q.units, ["my app.service"]);
        assert_eq!(q.text, ["unit:x y", "out of memory"]);
    }

    #[test]
    fn boots_count_back_or_name_an_id() {
        let id = "0123456789abcdef0123456789abcdef";
        let q = parse(&format!("boot:0 boot:-2 boot:{id}"));
        assert_eq!(q.boots, [BootRef::Back(0), BootRef::Back(2), BootRef::Id(id.into())]);
    }

    #[test]
    fn times_are_relative_to_now_or_absolute_in_the_given_zone() {
        assert_eq!(parse("since:1h").since_us, Some(NOW - 3600 * 1_000_000));
        assert_eq!(parse("until:2d").until_us, Some(NOW - 2 * 86400 * 1_000_000));
        let utc = 1_792_094_400_000_000;
        assert_eq!(parse(r#"since:"2026-10-15 20:00""#).since_us, Some(utc));
        assert_eq!(parse("since:2026-10-15T20:00:30").since_us, Some(utc + 30_000_000));
        let east = Query::parse("since:2026-10-15T20:00", NOW, &TimeZone::fixed(jiff::tz::offset(2)));
        assert_eq!(east.since_us, Some(utc - 2 * 3600 * 1_000_000));
    }

    #[test]
    fn only_text_and_pids_need_the_streaming_scan() {
        assert_eq!(parse("unit:a prio:err cat:kernel").search(), None);
        let s = parse("Disk pid:7").search().unwrap();
        assert_eq!((s.terms, s.pids), (vec!["disk".to_string()], vec![7]));
        assert!(parse("").is_empty());
    }
}
