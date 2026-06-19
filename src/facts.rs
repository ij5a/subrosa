//! Curated facts: the rows MEMORY.md is generated from. Used by the checkpoint
//! skills and by hand. Corrections soft-delete (status/superseded_at), never rm.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;

use clap::ValueEnum;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};

use crate::{db, paths};

#[derive(ValueEnum, Clone, Copy)]
pub enum FactAction {
    Upsert,
    Archive,
    Pin,
    Unpin,
    List,
    Link,
    Search,
}

#[derive(ValueEnum, Clone, Copy, PartialEq)]
pub enum StatusFilter {
    Active,
    Archived,
    All,
}

/// Ranking weight by fact type — guardrails/identity outrank look-ups when the
/// index is over budget.
pub fn type_weight(t: Option<&str>) -> i64 {
    match t.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "feedback" | "user" => 40,
        "project" => 20,
        "reference" => 10,
        _ => 15,
    }
}

/// One fat hook line would crowd several facts out of the byte-budgeted index.
pub const HOOK_MAX_CHARS: usize = 240;

/// Char-safe cap for index hook lines (multi-byte text never splits).
pub fn cap_hook(s: &str) -> String {
    if s.chars().count() <= HOOK_MAX_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(HOOK_MAX_CHARS - 1).collect();
        out.push('…');
        out
    }
}

/// Drop one matching pair of surrounding quotes — YAML quoting that shouldn't
/// leak into the stored value.
fn unquote(v: &str) -> String {
    let v = v.trim();
    let b = v.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// YAML-ish frontmatter reader for leaf files: only the keys we store, first
/// occurrence wins (so a nested `metadata: type:` can't override a flat one).
pub fn parse_frontmatter(text: &str) -> HashMap<String, String> {
    let mut fm = HashMap::new();
    if !text.starts_with("---") {
        return fm;
    }
    let body = &text[3..];
    let block = match body.find("\n---") {
        Some(end) => &body[..end],
        None => body,
    };
    for line in block.lines() {
        let trimmed = line.trim_start();
        for key in [
            "name",
            "type",
            "description",
            "originSessionId",
            "pinned",
            "tags",
        ] {
            if let Some(rest) = trimmed.strip_prefix(key) {
                if let Some(value) = rest.strip_prefix(':') {
                    fm.entry(key.to_string()).or_insert_with(|| unquote(value));
                    break;
                }
            }
        }
    }
    fm
}

/// The leaf body after the YAML frontmatter block (or the whole text if there's
/// no frontmatter). Wiki links are scanned here, never in the frontmatter.
fn body_after_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---") else {
        return text;
    };
    let Some(end) = rest.find("\n---") else {
        return text;
    };
    // Skip past the closing `---` line to the body that follows it.
    let after_marker = &rest[end + 1..];
    match after_marker.find('\n') {
        Some(nl) => &after_marker[nl + 1..],
        None => "",
    }
}

/// `[[slug]]` link targets in a leaf body, first-seen order, de-duplicated
/// case-insensitively. Links inside ``` fenced code blocks are skipped — those
/// are examples, not real cross-references.
pub fn extract_wiki_links(body: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[\[([^\[\]\n]+)\]\]").unwrap());
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        for cap in re.captures_iter(line) {
            let slug = cap[1].trim().to_string();
            if !slug.is_empty() && seen.insert(slug.to_ascii_lowercase()) {
                out.push(slug);
            }
        }
    }
    out
}

/// Infer a fact type from the leaf filename prefix.
pub fn guess_type(leaf_name: &str) -> &'static str {
    ["feedback", "project", "reference", "user"]
        .into_iter()
        .find(|t| leaf_name.starts_with(t))
        .unwrap_or("reference")
}

/// The encoded project name a memdir belongs to: its parent directory's name.
pub fn project_of(memdir: &Path) -> String {
    memdir
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

struct Existing {
    id: i64,
    type_: Option<String>,
    title: Option<String>,
    hook: Option<String>,
    description: Option<String>,
}

/// Insert or update one fact. Precedence: an explicit flag always wins;
/// otherwise an update keeps the stored value and only falls back to
/// frontmatter/slug for a new fact (or when the stored value is empty) — so a
/// no-flag update can't rewrite a curated title into the `name` slug.
#[allow(clippy::too_many_arguments)]
fn upsert(
    conn: &Connection,
    project: &str,
    memdir: &Path,
    leaf: &str,
    type_: Option<&str>,
    title: Option<&str>,
    hook: Option<&str>,
    pin: bool,
    origin_session: Option<&str>,
) -> rusqlite::Result<()> {
    let origin_session: Option<String> = origin_session.map(str::to_string).or_else(|| {
        std::env::var("SUBROSA_ORIGIN_SESSION")
            .ok()
            .filter(|s| !s.is_empty())
    });
    let fm = std::fs::read_to_string(memdir.join(leaf))
        .map(|t| parse_frontmatter(&t))
        .unwrap_or_default();
    let now = db::now_iso();
    let row: Option<Existing> = conn
        .query_row(
            "SELECT id, type, title, hook, description FROM facts WHERE project=? AND leaf_path=?",
            params![project, leaf],
            |r| {
                Ok(Existing {
                    id: r.get(0)?,
                    type_: r.get(1)?,
                    title: r.get(2)?,
                    hook: r.get(3)?,
                    description: r.get(4)?,
                })
            },
        )
        .optional()?;

    let type_ = type_
        .map(str::to_string)
        .or_else(|| fm.get("type").cloned().filter(|s| !s.is_empty()))
        .or_else(|| {
            row.as_ref()
                .and_then(|r| r.type_.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| guess_type(leaf).to_string());
    let title = title
        .map(str::to_string)
        .or_else(|| {
            row.as_ref()
                .and_then(|r| r.title.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| {
            fm.get("name")
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    Path::new(leaf)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .replace('_', " ")
        });
    let hook = hook
        .map(str::to_string)
        .or_else(|| {
            row.as_ref()
                .and_then(|r| r.hook.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| fm.get("description").cloned().unwrap_or_default());
    let capped = cap_hook(&hook);
    if capped != hook {
        eprintln!("[subrosa] hook truncated to {HOOK_MAX_CHARS} chars — index lines stay short");
    }
    let hook = capped;
    let description: Option<String> = fm
        .get("description")
        .cloned()
        .or_else(|| row.as_ref().and_then(|r| r.description.clone()));

    let action = if let Some(r) = &row {
        conn.execute(
            "UPDATE facts SET type=?, title=?, hook=?, description=?, status='active', \
             superseded_at=NULL, pinned=MAX(pinned,?), updated_at=? WHERE id=?",
            params![type_, title, hook, description, pin as i64, now, r.id],
        )?;
        "updated"
    } else {
        // New facts append to the end of the curated order; updates keep position.
        let nxt: i64 = conn.query_row(
            "SELECT COALESCE(MAX(index_seq), -1) + 1 FROM facts WHERE project=?",
            [project],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO facts (project,name,type,title,hook,leaf_path,description,index_seq,\
             pinned,status,origin_session,created_at,updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,'active',?,?,?)",
            params![
                project,
                fm.get("name"),
                type_,
                title,
                hook,
                leaf,
                description,
                nxt,
                pin as i64,
                origin_session,
                now,
                now
            ],
        )?;
        "inserted"
    };
    println!("[subrosa] {action}: {type_}: {title} ({leaf})");
    Ok(())
}

// (type, title, leaf_path, pinned, status)
type FactListRow = (Option<String>, Option<String>, Option<String>, i64, String);

/// The `📌`/`🗄` flag prefix shared by `fact list` and `fact search`.
fn fact_flags(pinned: i64, status: &str) -> String {
    format!(
        "{}{}",
        if pinned != 0 { "📌" } else { "  " },
        if status == "active" { " " } else { "🗄 " }
    )
}

fn list(conn: &Connection, project: &str, status: StatusFilter) -> rusqlite::Result<()> {
    let (where_sql, binds): (&str, Vec<String>) = match status {
        StatusFilter::All => ("project=?", vec![project.to_string()]),
        StatusFilter::Active => (
            "project=? AND status=?",
            vec![project.to_string(), "active".into()],
        ),
        StatusFilter::Archived => (
            "project=? AND status=?",
            vec![project.to_string(), "archived".into()],
        ),
    };
    let sql = format!(
        "SELECT type,title,leaf_path,pinned,status FROM facts WHERE {where_sql} \
         ORDER BY index_seq IS NULL, index_seq"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<FactListRow> = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    for (type_, title, leaf, pinned, st) in rows {
        println!(
            "{} [{}] {} ({})",
            fact_flags(pinned, &st),
            type_.as_deref().unwrap_or(""),
            title.as_deref().unwrap_or(""),
            leaf.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

/// `subrosa fact search <query>` — bm25-ranked full-text search over the curated
/// facts (title/hook/description via facts_fts), scoped to one project. Read-only.
fn search_facts(
    query: Option<String>,
    project: Option<String>,
    memdir: Option<PathBuf>,
    status: StatusFilter,
) -> ExitCode {
    // No archive yet → no facts to search. Quiet, like `link`.
    let Ok(conn) = db::connect_readonly() else {
        println!("[subrosa] no facts yet");
        return ExitCode::SUCCESS;
    };
    let memdir = memdir
        .map(|p| paths::expanduser(&p))
        .unwrap_or_else(|| db::current_memdir(Some(&conn)));
    let project = project.unwrap_or_else(|| project_of(&memdir));
    let Some(query) = query.filter(|s| !s.trim().is_empty()) else {
        eprintln!("[subrosa] fact search needs a query");
        return ExitCode::from(2);
    };
    // Reuse the turns-search term quoting so identifiers like `cache-prod` match
    // instead of tripping FTS5's operators. Facts have no trigram index, so no --fuzzy.
    let terms: Vec<String> = query.split_whitespace().map(str::to_string).collect();
    let m = crate::search::build_match(&terms, false, false);

    let mut sql = String::from(
        "SELECT f.type, f.title, f.leaf_path, f.pinned, f.status, f.hook \
         FROM facts_fts JOIN facts f ON f.id = facts_fts.rowid \
         WHERE facts_fts MATCH ?1 AND f.project = ?2",
    );
    let mut binds: Vec<String> = vec![m.clone(), project];
    if status != StatusFilter::All {
        sql.push_str(" AND f.status = ?3");
        binds.push(match status {
            StatusFilter::Archived => "archived".into(),
            _ => "active".into(),
        });
    }
    sql.push_str(" ORDER BY bm25(facts_fts)");

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[subrosa] fact search query error: {e}");
            return ExitCode::FAILURE;
        }
    };
    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        String,
        Option<String>,
    );
    let rows: Result<Vec<Row>, _> = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .and_then(|it| it.collect());
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[subrosa] fact search query error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if rows.is_empty() {
        println!("[subrosa] no facts match: {m}");
        return ExitCode::SUCCESS;
    }
    for (type_, title, leaf, pinned, st, hook) in &rows {
        println!(
            "{} [{}] {} ({})",
            fact_flags(*pinned, st),
            type_.as_deref().unwrap_or(""),
            title.as_deref().unwrap_or(""),
            leaf.as_deref().unwrap_or("")
        );
        // The hook is the curated one-liner — the content the match is usually in.
        if let Some(h) = hook.as_deref().filter(|s| !s.is_empty()) {
            println!("      {h}");
        }
    }
    println!("\n[subrosa] {} fact(s) match", rows.len());
    ExitCode::SUCCESS
}

/// One curated fact, reduced to what `fact link` needs.
struct LinkFact {
    name: Option<String>,
    type_: Option<String>,
    title: Option<String>,
    hook: Option<String>,
    leaf_path: String,
    status: String,
}

impl LinkFact {
    /// The slug other leaves resolve against: the `name` slug, or the leaf
    /// filename without `.md` when `name` is absent.
    fn slug(&self) -> String {
        self.name
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| leaf_stem(&self.leaf_path))
    }
    /// The one-line label next to a link: hook, then title, then slug.
    fn label(&self) -> String {
        let h = self.hook.as_deref().filter(|s| !s.is_empty());
        let t = self.title.as_deref().filter(|s| !s.is_empty());
        h.or(t).map(str::to_string).unwrap_or_else(|| self.slug())
    }
    /// `[type]` tag, with `, archived` appended for soft-deleted facts.
    fn tag(&self) -> String {
        let t = self.type_.as_deref().unwrap_or("?");
        if self.status == "active" {
            t.to_string()
        } else {
            format!("{t}, archived")
        }
    }
}

/// `reference_foo.md` → `reference_foo`.
fn leaf_stem(leaf: &str) -> String {
    leaf.strip_suffix(".md").unwrap_or(leaf).to_string()
}

/// Normalize a written link target or anchor argument to its resolution key.
fn link_key(s: &str) -> String {
    s.trim().trim_end_matches(".md").to_ascii_lowercase()
}

fn load_link_facts(conn: &Connection, project: &str) -> rusqlite::Result<Vec<LinkFact>> {
    let mut stmt = conn.prepare(
        "SELECT name, type, title, hook, leaf_path, status FROM facts \
         WHERE project=? AND leaf_path IS NOT NULL \
         ORDER BY index_seq IS NULL, index_seq, leaf_path",
    )?;
    let rows = stmt
        .query_map([project], |r| {
            Ok(LinkFact {
                name: r.get(0)?,
                type_: r.get(1)?,
                title: r.get(2)?,
                hook: r.get(3)?,
                leaf_path: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                status: r
                    .get::<_, Option<String>>(5)?
                    .unwrap_or_else(|| "active".into()),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|f| !f.leaf_path.is_empty())
        .collect())
}

/// `subrosa fact link <anchor>` — the curated `[[slug]]` links into and out of
/// one fact, flagging links that resolve to nothing. Read-only.
fn link(anchor: Option<String>, project: Option<String>, memdir: Option<PathBuf>) -> ExitCode {
    // No archive yet → nothing to link. Quiet, like `related`.
    let Ok(conn) = db::connect_readonly() else {
        println!("[subrosa] no facts yet");
        return ExitCode::SUCCESS;
    };
    let memdir = memdir
        .map(|p| paths::expanduser(&p))
        .unwrap_or_else(|| db::current_memdir(Some(&conn)));
    let project = project.unwrap_or_else(|| project_of(&memdir));
    let Some(anchor) = anchor else {
        eprintln!("[subrosa] fact link needs an anchor (a fact slug or leaf filename)");
        return ExitCode::from(2);
    };

    let facts = match load_link_facts(&conn, &project) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[subrosa] fact link query failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Resolve every slug/stem (lowercased) to its fact. `name` wins over a
    // colliding leaf-stem, so seed stems first, then overwrite with names.
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (i, f) in facts.iter().enumerate() {
        by_key.insert(leaf_stem(&f.leaf_path).to_ascii_lowercase(), i);
    }
    for (i, f) in facts.iter().enumerate() {
        if let Some(n) = f.name.as_deref().filter(|s| !s.is_empty()) {
            by_key.insert(n.to_ascii_lowercase(), i);
        }
    }

    let Some(&anchor_idx) = by_key.get(&link_key(&anchor)) else {
        println!("[subrosa] no fact matches \"{anchor}\"");
        return ExitCode::SUCCESS;
    };

    // Each fact's outbound links, extracted once (the anchor's own set included).
    let links: Vec<Vec<String>> = facts
        .iter()
        .map(|f| {
            std::fs::read_to_string(memdir.join(&f.leaf_path))
                .map(|t| extract_wiki_links(body_after_frontmatter(&t)))
                .unwrap_or_default()
        })
        .collect();

    // The anchor's identity keys, for matching inbound links by name or stem.
    let anchor_fact = &facts[anchor_idx];
    let anchor_keys: HashSet<String> = [
        anchor_fact
            .name
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase()),
        Some(leaf_stem(&anchor_fact.leaf_path).to_ascii_lowercase()),
    ]
    .into_iter()
    .flatten()
    .collect();

    render_links(&facts, anchor_idx, &by_key, &links, &anchor_keys);
    ExitCode::SUCCESS
}

fn render_links(
    facts: &[LinkFact],
    anchor_idx: usize,
    by_key: &HashMap<String, usize>,
    links: &[Vec<String>],
    anchor_keys: &HashSet<String>,
) {
    println!("links for «{}»\n", facts[anchor_idx].slug());

    // Outbound: the anchor's own [[...]] links, in document order.
    println!("links to (outbound):");
    let mut outbound = 0usize;
    let mut dangling = 0usize;
    if links[anchor_idx].is_empty() {
        println!("  (none)");
    }
    for written in &links[anchor_idx] {
        outbound += 1;
        match by_key.get(&link_key(written)) {
            Some(&i) if i == anchor_idx => println!("  → {}  [self]", facts[i].slug()),
            Some(&i) => println!(
                "  → {}  [{}] {}",
                facts[i].slug(),
                facts[i].tag(),
                facts[i].label()
            ),
            None => {
                dangling += 1;
                println!("  → {}  [dangling]", written.trim());
            }
        }
    }

    // Inbound: other facts whose bodies link to the anchor (by name or stem).
    println!("\nlinked from (inbound):");
    let mut inbound = 0usize;
    for (i, f) in facts.iter().enumerate() {
        if i == anchor_idx {
            continue;
        }
        if links[i].iter().any(|w| anchor_keys.contains(&link_key(w))) {
            inbound += 1;
            println!("  ← {}  [{}] {}", f.slug(), f.tag(), f.label());
        }
    }
    if inbound == 0 {
        println!("  (none)");
    }

    let dang = if dangling > 0 {
        format!(" ({dangling} dangling)")
    } else {
        String::new()
    };
    println!("\n[subrosa] {outbound} outbound{dang}, {inbound} inbound");
}

/// `subrosa fact <action> ...` entry point.
#[allow(clippy::too_many_arguments)]
pub fn run(
    action: FactAction,
    leaf: Option<String>,
    type_: Option<String>,
    title: Option<String>,
    hook: Option<String>,
    pin: bool,
    origin_session: Option<String>,
    project: Option<String>,
    memdir: Option<PathBuf>,
    status: StatusFilter,
    anchor: Option<String>,
) -> ExitCode {
    // `link` and `search` are read-only — open read-only before the read-write connect below.
    if let FactAction::Link = action {
        return link(anchor, project, memdir);
    }
    if let FactAction::Search = action {
        return search_facts(anchor, project, memdir, status);
    }
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    let memdir = memdir
        .map(|p| paths::expanduser(&p))
        .unwrap_or_else(|| db::current_memdir(Some(&conn)));
    let project = project.unwrap_or_else(|| project_of(&memdir));

    if let FactAction::List = action {
        return match list(&conn, &project, status) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[subrosa] list failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let Some(leaf) = leaf else {
        eprintln!("[subrosa] this action needs --leaf");
        return ExitCode::from(2);
    };
    if leaf.contains('/') || leaf.contains('\\') || leaf.contains("..") {
        eprintln!("[subrosa] refusing suspicious leaf name (must be a bare filename): {leaf:?}");
        return ExitCode::from(2);
    }

    let now = db::now_iso();
    let result = match action {
        FactAction::Upsert => upsert(
            &conn,
            &project,
            &memdir,
            &leaf,
            type_.as_deref(),
            title.as_deref(),
            hook.as_deref(),
            pin,
            origin_session.as_deref(),
        ),
        FactAction::Archive => conn
            .execute(
                "UPDATE facts SET status='archived', superseded_at=?, updated_at=? \
                 WHERE project=? AND leaf_path=?",
                params![now, now, project, leaf],
            )
            .map(|n| {
                if n > 0 {
                    println!("[subrosa] archived {n} fact(s): {leaf}");
                } else {
                    println!("[subrosa] no fact for {leaf}");
                }
            }),
        FactAction::Pin => conn
            .execute(
                "UPDATE facts SET pinned=1, updated_at=? WHERE project=? AND leaf_path=?",
                params![now, project, leaf],
            )
            .map(|n| println!("[subrosa] pinned {n} fact(s): {leaf}")),
        FactAction::Unpin => conn
            .execute(
                "UPDATE facts SET pinned=0, updated_at=? WHERE project=? AND leaf_path=?",
                params![now, project, leaf],
            )
            .map(|n| println!("[subrosa] unpinned {n} fact(s): {leaf}")),
        FactAction::List | FactAction::Link | FactAction::Search => unreachable!(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[subrosa] fact {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_links_dedup_and_first_seen_order() {
        // [[Alpha]] dedups against [[alpha]]; first-seen casing is kept.
        let got = extract_wiki_links("to [[alpha]] then [[beta]], and [[Alpha]] again");
        assert_eq!(got, ["alpha", "beta"].map(String::from));
    }

    #[test]
    fn extract_links_skips_code_fences() {
        let got = extract_wiki_links("real [[alpha]]\n```\n[[fenced]]\n```\nafter [[beta]]");
        assert_eq!(got, ["alpha", "beta"].map(String::from));
    }

    #[test]
    fn extract_links_ignores_empty_and_nonlinks() {
        assert!(extract_wiki_links("just [single] brackets, nothing here").is_empty());
        assert!(extract_wiki_links("empty [[]] and [[   ]] targets").is_empty());
    }

    #[test]
    fn body_after_frontmatter_strips_yaml_block() {
        let leaf = "---\nname: foo\ndescription: x\n---\nbody [[bar]] here\n";
        assert_eq!(body_after_frontmatter(leaf), "body [[bar]] here\n");
        assert_eq!(body_after_frontmatter("plain [[bar]]"), "plain [[bar]]");
    }
}
