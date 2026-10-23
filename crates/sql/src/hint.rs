//! Hints beside errors (PLAN.md §M14.10): what to do about an error, for
//! people and for models — the name that was probably meant, the names
//! there are, the cast that would work, the statement that ends a failed
//! transaction.  Sites that know more (the binder knows the columns in
//! scope) attach their own ([`Error::with_hint`]); [`complete`] gives the
//! rest a hint by their SQLSTATE.

use crate::error::Error;

/// The edit distance of `a` and `b` (case ignored): letters added,
/// removed, changed, or two neighbours swapped.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    d[0] = (0..=b.len()).collect();
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// Of `candidates`, the one closest to `name` — close enough to be what
/// was meant (a third of its letters or fewer changed, a letter changed in
/// a short one) — the first on a tie.
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let most = (name.chars().count() / 3).max(1);
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        let d = distance(name, c);
        if d > 0 && d <= most && best.is_none_or(|(b, _)| d < b) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c)
}

/// "Perhaps you meant …", or the names there are (when they are few), or
/// `otherwise`.
pub fn names(
    what: &str,
    name: &str,
    candidates: &[&str],
    otherwise: Option<&str>,
) -> Option<String> {
    if let Some(c) = closest(name, candidates.iter().copied()) {
        return Some(format!("Perhaps you meant {what} \"{c}\"."));
    }
    if !candidates.is_empty() && candidates.len() <= 12 {
        let mut list: Vec<&str> = candidates.to_vec();
        list.dedup();
        return Some(format!("The {what}s here: {}.", list.join(", ")));
    }
    otherwise.map(str::to_string)
}

/// A hint by `e`'s SQLSTATE, where it has none.
pub fn complete(mut e: Error) -> Error {
    if e.hint.is_some() {
        return e;
    }
    e.hint = match e.code {
        "25P02" => {
            Some("The transaction has failed: ROLLBACK ends it (its changes are undone).".into())
        }
        "40001" => Some("It conflicted with a concurrent transaction: run it again.".into()),
        "23502" => Some("Give the column a value (or the table a DEFAULT for it).".into()),
        "23505" => Some("A row with this key exists: update that row, or use another key.".into()),
        "42501" => Some(
            "The owner of the object, or an administrator, can grant it: GRANT … ON … TO ….".into(),
        ),
        "22003" => Some(
            "The value does not fit its type: a wider one (bigint, numeric) holds more.".into(),
        ),
        "42804" => Some(
            "Convert the value to the type wanted: CAST(value AS type), or value::type.".into(),
        ),
        "42883" if e.message.contains('(') || e.message.starts_with("operator") => Some(
            "No function or operator takes these argument types: cast an argument (value::type)."
                .into(),
        ),
        "42P07" => Some("Use another name, or IF NOT EXISTS to keep the one there.".into()),
        "22012" => Some("Guard the divisor: NULLIF(divisor, 0) gives NULL instead.".into()),
        "21000" => {
            Some("Make the subquery return one row (LIMIT 1, or an aggregate), or use IN.".into())
        }
        _ => None,
    };
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_meant() {
        assert_eq!(closest("emial", ["id", "email", "region"]), Some("email"));
        assert_eq!(
            closest("custmers", ["orders", "customers"]),
            Some("customers")
        );
        assert_eq!(closest("Regoin", ["id", "email", "region"]), Some("region"));
        assert_eq!(closest("x", ["id", "y"]), Some("y"));
        assert_eq!(closest("customer", ["orders", "products"]), None);
        assert_eq!(
            closest("id", ["id"]),
            None,
            "the same name is not a suggestion"
        );
        assert_eq!(
            names("column", "nme", &["id", "name"], None).as_deref(),
            Some("Perhaps you meant column \"name\".")
        );
        assert_eq!(
            names("column", "zzz", &["id", "name"], None).as_deref(),
            Some("The columns here: id, name.")
        );
    }
}
