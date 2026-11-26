//! Issue ids: `PREFIX-GROUP-NN`, rule `^[A-Z]{2,5}-[0-9]{1,5}-[0-9]{2}$`.
//! One database means one allocator: ids are never reused or renumbered
//! (deleted ids are retired, not freed).

use crate::error::{AitError, Result};
use rusqlite::Connection;

pub fn is_prefix(s: &str) -> bool {
    (2..=5).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// Splits a valid id into (prefix, group, number).
pub fn parse_id(s: &str) -> Option<(&str, u32, u32)> {
    let mut parts = s.split('-');
    let p = parts.next()?;
    let g = parts.next()?;
    let n = parts.next()?;
    if parts.next().is_some() || !is_prefix(p) {
        return None;
    }
    if !(1..=5).contains(&g.len()) || !g.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if n.len() != 2 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((p, g.parse().ok()?, n.parse().ok()?))
}

pub fn is_id(s: &str) -> bool {
    parse_id(s).is_some()
}

pub fn format_id(p: &str, g: u32, n: u32) -> String {
    format!("{p}-{g}-{n:02}")
}

fn check_prefix(prefix: &str, allowed: &[String]) -> Result<()> {
    if !is_prefix(prefix) {
        return Err(AitError::new(
            "INVALID_ID",
            format!("'{prefix}' is not a valid prefix (2-5 letters A-Z)"),
        ));
    }
    if !allowed.is_empty() && !allowed.iter().any(|a| a == prefix) {
        return Err(AitError::new(
            "UNKNOWN_PREFIX",
            format!(
                "prefix '{prefix}' is not one of the configured prefixes ({})",
                allowed.join(", ")
            ),
        ));
    }
    Ok(())
}

/// Ids in use or retired for `prefix`, as (group, number).
fn used(conn: &Connection, prefix: &str) -> Result<Vec<(u32, u32)>> {
    let like = format!("{prefix}-%");
    let mut out = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id FROM issues WHERE id LIKE ?1 UNION SELECT id FROM retired_ids WHERE id LIKE ?1",
    )?;
    let rows = stmt.query_map([&like], |r| r.get::<_, String>(0))?;
    for id in rows {
        let id = id?;
        if let Some((p, g, n)) = parse_id(&id)
            && p == prefix
        {
            out.push((g, n));
        }
    }
    Ok(out)
}

/// Groups of `prefix` whose `-00` issue is a feature, milestone or release.
fn container_groups(conn: &Connection, prefix: &str) -> Result<Vec<u32>> {
    let like = format!("{prefix}-%-00");
    let mut stmt = conn.prepare(
        "SELECT id FROM issues WHERE id LIKE ?1 AND type IN ('feature', 'milestone', 'release')",
    )?;
    let rows = stmt.query_map([&like], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for id in rows {
        if let Some((p, g, 0)) = parse_id(&id?)
            && p == prefix
        {
            out.push(g);
        }
    }
    Ok(out)
}

fn taken(conn: &Connection, id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM issues WHERE id = ?1) + (SELECT COUNT(*) FROM retired_ids WHERE id = ?1)",
        [id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// What the new issue is, for picking its group from a bare prefix.
#[derive(Default, Clone, Copy)]
pub struct Hint<'a> {
    /// Its parent: a child takes the next number in its parent's group.
    pub parent: Option<&'a str>,
    /// A feature, milestone or release opens a new group (`CORE-2-00`); the
    /// numbers after `-00` are for the issues under it.
    pub container: bool,
}

/// Picks a new id. `spec` is a prefix (`CORE`), a group (`CORE-2`) or a full
/// id (`CORE-1-05`). `after` picks the next free number after that id. For a
/// bare prefix, `hint` decides the group (see `Hint`); otherwise the newest
/// group is used.
pub fn allocate(
    conn: &Connection,
    allowed: &[String],
    spec: &str,
    after: Option<&str>,
    hint: Hint,
) -> Result<String> {
    let spec = spec.trim().to_ascii_uppercase();
    if let Some((p, _, _)) = parse_id(&spec) {
        check_prefix(p, allowed)?;
        if taken(conn, &spec)? {
            return Err(AitError::new(
                "ID_TAKEN",
                format!("{spec} is already used (or was used by a deleted issue)"),
            ));
        }
        return Ok(spec);
    }
    let (prefix, group) = match spec.split_once('-') {
        Some((p, g)) => {
            let g: u32 = g.parse().ok().filter(|g| *g <= 99_999).ok_or_else(|| {
                AitError::new(
                    "INVALID_ID",
                    format!("'{spec}' is not PREFIX, PREFIX-N or an id"),
                )
            })?;
            (p.to_string(), Some(g))
        }
        None => (spec.clone(), None),
    };
    check_prefix(&prefix, allowed)?;
    let used = used(conn, &prefix)?;

    // `--after` in the same prefix (and group, if given) picks the next free
    // number after it; otherwise it only affects ordering, not the id.
    let same_group = after
        .and_then(parse_id)
        .filter(|(ap, ag, _)| *ap == prefix && group.is_none_or(|g| g == *ag));
    if let (Some(a), Some((_, ag, an))) = (after, same_group) {
        for n in an + 1..=99 {
            if !used.contains(&(ag, n)) {
                return Ok(format_id(&prefix, ag, n));
            }
        }
        return Err(AitError::new(
            "GROUP_FULL",
            format!("no free id after {a} in group {prefix}-{ag}"),
        ));
    }

    if group.is_none() {
        let parent_group = hint
            .parent
            .and_then(parse_id)
            .filter(|(pp, _, _)| *pp == prefix)
            .map(|(_, pg, _)| pg);
        if let Some(pg) = parent_group {
            return match used.iter().filter(|x| x.0 == pg).map(|x| x.1).max() {
                None => Ok(format_id(&prefix, pg, 0)),
                Some(n) if n < 99 => Ok(format_id(&prefix, pg, n + 1)),
                Some(_) => Err(AitError::new(
                    "GROUP_FULL",
                    format!(
                        "group {prefix}-{pg} (of the parent) has no free numbers left; pass a group or id explicitly"
                    ),
                )),
            };
        }
        if hint.container {
            return match used.iter().map(|x| x.0).max() {
                None => Ok(format_id(&prefix, 1, 0)),
                Some(g) if g < 99_999 => Ok(format_id(&prefix, g + 1, 0)),
                Some(_) => Err(AitError::new(
                    "GROUP_FULL",
                    format!("prefix {prefix} has no free groups left"),
                )),
            };
        }
    }
    // A loose issue (no parent in this prefix, not a container) never lands
    // in a group opened by a feature, milestone or release: those numbers
    // belong to the issues under it. It goes to the newest other group, or
    // opens a new one.
    let newest = used.iter().map(|x| x.0).max();
    let g = match group {
        Some(g) => g,
        None => {
            let reserved = container_groups(conn, &prefix)?;
            match used
                .iter()
                .map(|x| x.0)
                .filter(|g| !reserved.contains(g))
                .max()
            {
                Some(g) => g,
                None => newest.map_or(1, |n| n + 1),
            }
        }
    };
    match used.iter().filter(|x| x.0 == g).map(|x| x.1).max() {
        None => Ok(format_id(&prefix, g, 0)),
        Some(n) if n < 99 => Ok(format_id(&prefix, g, n + 1)),
        Some(_) if group.is_none() && newest.is_some_and(|n| n < 99_999) => {
            Ok(format_id(&prefix, newest.map_or(1, |n| n + 1), 0))
        }
        Some(_) => Err(AitError::new(
            "GROUP_FULL",
            format!("group {prefix}-{g} has no free numbers left"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_rule() {
        assert!(is_id("CORE-1-00"));
        assert!(is_id("DES-22222-99"));
        assert!(!is_id("core-1-00"));
        assert!(!is_id("C-1-00"));
        assert!(!is_id("TOOLONG-1-00"));
        assert!(!is_id("CORE-1-0"));
        assert!(!is_id("CORE-123456-00"));
        assert!(!is_id("CORE-1-00-1"));
        assert!(!is_id("CORE--00"));
    }

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE issues (id TEXT PRIMARY KEY, type TEXT NOT NULL DEFAULT 'task'); CREATE TABLE retired_ids (id TEXT PRIMARY KEY);",
        )
        .unwrap();
        c
    }

    #[test]
    fn allocates_in_sequence_and_gaps() {
        let c = db();
        assert_eq!(
            allocate(&c, &[], "core", None, Hint::default()).unwrap(),
            "CORE-1-00"
        );
        c.execute_batch("INSERT INTO issues(id) VALUES ('CORE-1-00'),('CORE-1-10'),('CORE-1-11');")
            .unwrap();
        assert_eq!(
            allocate(&c, &[], "CORE", None, Hint::default()).unwrap(),
            "CORE-1-12"
        );
        assert_eq!(
            allocate(&c, &[], "CORE", Some("CORE-1-10"), Hint::default()).unwrap(),
            "CORE-1-12"
        );
        assert_eq!(
            allocate(&c, &[], "CORE", Some("CORE-1-00"), Hint::default()).unwrap(),
            "CORE-1-01"
        );
        assert_eq!(
            allocate(&c, &[], "CORE-2", None, Hint::default()).unwrap(),
            "CORE-2-00"
        );
        // --after in another prefix only affects ordering.
        assert_eq!(
            allocate(&c, &[], "PAP", Some("CORE-1-10"), Hint::default()).unwrap(),
            "PAP-1-00"
        );
        c.execute_batch("INSERT INTO retired_ids VALUES ('CORE-1-12');")
            .unwrap();
        assert_eq!(
            allocate(&c, &[], "CORE", None, Hint::default()).unwrap(),
            "CORE-1-13"
        );
        assert_eq!(
            allocate(&c, &[], "CORE-1-12", None, Hint::default())
                .unwrap_err()
                .code,
            "ID_TAKEN"
        );
    }

    #[test]
    fn rolls_over_to_next_group() {
        let c = db();
        c.execute_batch("INSERT INTO issues(id) VALUES ('PAP-3-99');")
            .unwrap();
        assert_eq!(
            allocate(&c, &[], "PAP", None, Hint::default()).unwrap(),
            "PAP-4-00"
        );
        assert_eq!(
            allocate(&c, &[], "PAP-3", None, Hint::default())
                .unwrap_err()
                .code,
            "GROUP_FULL"
        );
    }

    #[test]
    fn features_open_a_group_and_children_join_their_parent() {
        let c = db();
        let feature = Hint {
            parent: None,
            container: true,
        };
        assert_eq!(
            allocate(&c, &[], "CORE", None, feature).unwrap(),
            "CORE-1-00"
        );
        c.execute_batch("INSERT INTO issues(id) VALUES ('CORE-1-00');")
            .unwrap();
        assert_eq!(
            allocate(&c, &[], "CORE", None, feature).unwrap(),
            "CORE-2-00"
        );
        c.execute_batch("INSERT INTO issues(id) VALUES ('CORE-2-00');")
            .unwrap();
        let under = |p| Hint {
            parent: Some(p),
            container: false,
        };
        assert_eq!(
            allocate(&c, &[], "CORE", None, under("CORE-1-00")).unwrap(),
            "CORE-1-01"
        );
        c.execute_batch("INSERT INTO issues(id) VALUES ('CORE-1-01'),('CORE-1-02');")
            .unwrap();
        assert_eq!(
            allocate(&c, &[], "CORE", None, under("CORE-1-00")).unwrap(),
            "CORE-1-03"
        );
        assert_eq!(
            allocate(&c, &[], "CORE", None, under("CORE-2-00")).unwrap(),
            "CORE-2-01"
        );
        // A sub-feature under a feature joins the parent's group too.
        let sub = Hint {
            parent: Some("CORE-2-00"),
            container: true,
        };
        assert_eq!(allocate(&c, &[], "CORE", None, sub).unwrap(), "CORE-2-01");
        // A parent with another prefix does not choose the group.
        assert_eq!(
            allocate(&c, &[], "PAP", None, under("CORE-1-00")).unwrap(),
            "PAP-1-00"
        );
        // An explicit group or id still wins.
        assert_eq!(
            allocate(&c, &[], "CORE-7", None, under("CORE-1-00")).unwrap(),
            "CORE-7-00"
        );
        // A full parent group is an error, not a silent jump elsewhere.
        c.execute_batch("INSERT INTO issues(id) VALUES ('CORE-3-99');")
            .unwrap();
        assert_eq!(
            allocate(&c, &[], "CORE", None, under("CORE-3-99"))
                .unwrap_err()
                .code,
            "GROUP_FULL"
        );
    }

    #[test]
    fn loose_issues_stay_out_of_feature_groups() {
        let c = db();
        c.execute_batch(
            "INSERT INTO issues VALUES ('CORE-1-00', 'feature'), ('CORE-1-01', 'task');",
        )
        .unwrap();
        // Only feature groups exist: a loose task opens a new group.
        assert_eq!(
            allocate(&c, &[], "CORE", None, Hint::default()).unwrap(),
            "CORE-2-00"
        );
        c.execute_batch(
            "INSERT INTO issues VALUES ('CORE-2-00', 'task'), ('CORE-3-00', 'release');",
        )
        .unwrap();
        // The newest group that is not a container's is used.
        assert_eq!(
            allocate(&c, &[], "CORE", None, Hint::default()).unwrap(),
            "CORE-2-01"
        );
        // When that group is full, a new group after the newest one opens.
        c.execute_batch("INSERT INTO issues(id) VALUES ('CORE-2-99');")
            .unwrap();
        assert_eq!(
            allocate(&c, &[], "CORE", None, Hint::default()).unwrap(),
            "CORE-4-00"
        );
    }

    #[test]
    fn enforces_configured_prefixes() {
        let c = db();
        let allowed = vec!["CORE".to_string()];
        assert_eq!(
            allocate(&c, &allowed, "PAP", None, Hint::default())
                .unwrap_err()
                .code,
            "UNKNOWN_PREFIX"
        );
        assert!(allocate(&c, &allowed, "CORE", None, Hint::default()).is_ok());
    }
}
