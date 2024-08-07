use std::cmp::max;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::Read;
use std::io::Write as IoWrite;
use std::process::Command;

use ansi_term::Style;
use chrono::offset::TimeZone;
use clap::{App, AppSettings, Arg, ArgGroup, ArgMatches, SubCommand};
use git2::{Commit, Config, Delta, Diff, Object, ObjectType, Oid, Reference, Repository, Tree, TreeBuilder};
use quick_error::quick_error;

quick_error! {
    #[derive(Debug)]
    enum Error {
        Git2(err: git2::Error) {
            from()
            cause(err)
            display("{}", err)
        }
        IO(err: std::io::Error) {
            from()
            cause(err)
            display("{}", err)
        }
        Munkres(err: munkres::Error) {
            from()
            display("{:?}", err)
        }
        Msg(msg: String) {
            from()
            from(s: &'static str) -> (s.to_string())
            description(msg)
            display("{}", msg)
        }
        Utf8Error(err: std::str::Utf8Error) {
            from()
            cause(err)
            display("{}", err)
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

const COMMIT_MESSAGE_COMMENT: &str = "
# Please enter the commit message for your changes. Lines starting
# with '#' will be ignored, and an empty message aborts the commit.
";
const COVER_LETTER_COMMENT: &str = "
# Please enter the cover letter for your changes. Lines starting
# with '#' will be ignored, and an empty message aborts the change.
";
const REBASE_COMMENT: &str = "\
#
# Commands:
# p, pick = use commit
# r, reword = use commit, but edit the commit message
# e, edit = use commit, but stop for amending
# s, squash = use commit, but meld into previous commit
# f, fixup = like \"squash\", but discard this commit's log message
# x, exec = run command (the rest of the line) using shell
# d, drop = remove commit
#
# These lines can be re-ordered; they are executed from top to bottom.
#
# If you remove a line here THAT COMMIT WILL BE LOST.
#
# However, if you remove everything, the rebase will be aborted.
";
const SCISSOR_LINE: &str = "\
# ------------------------ >8 ------------------------";
const SCISSOR_COMMENT: &str = "\
# Do not touch the line above.
# Everything below will be removed.
";

const SHELL_METACHARS: &str = "|&;<>()$`\\\"' \t\n*?[#~=%";

const SERIES_PREFIX: &str = "refs/heads/git-series/";
const SHEAD_REF: &str = "refs/SHEAD";
const STAGED_PREFIX: &str = "refs/git-series-internals/staged/";
const WORKING_PREFIX: &str = "refs/git-series-internals/working/";

const GIT_FILEMODE_BLOB: u32 = 0o100644;
const GIT_FILEMODE_COMMIT: u32 = 0o160000;

fn commit_obj_summarize_components(commit: &mut Commit) -> Result<(String, String)> {
    let short_id_buf = commit.as_object().short_id()?;
    let short_id = short_id_buf.as_str().unwrap();
    let summary = String::from_utf8_lossy(commit.summary_bytes().unwrap());
    Ok((short_id.to_string(), summary.to_string()))
}

fn commit_summarize_components(repo: &Repository, id: Oid) -> Result<(String, String)> {
    let mut commit = repo.find_commit(id)?;
    commit_obj_summarize_components(&mut commit)
}

fn commit_obj_summarize(commit: &mut Commit) -> Result<String> {
    let (short_id, summary) = commit_obj_summarize_components(commit)?;
    Ok(format!("{} {}", short_id, summary))
}

fn commit_summarize(repo: &Repository, id: Oid) -> Result<String> {
    let mut commit = repo.find_commit(id)?;
    commit_obj_summarize(&mut commit)
}

fn notfound_to_none<T>(result: std::result::Result<T, git2::Error>) -> Result<Option<T>> {
    match result {
        Err(ref e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(x) => Ok(Some(x)),
    }
}

// If current_id_opt is Some, acts like reference_matching.  If current_id_opt is None, acts like
// reference.
fn reference_matching_opt<'repo>(
    repo: &'repo Repository,
    name: &str,
    id: Oid,
    force: bool,
    current_id_opt: Option<Oid>,
    log_message: &str,
) -> Result<Reference<'repo>> {
    Ok(match current_id_opt {
        None => repo.reference(name, id, force, log_message)?,
        Some(current_id) => repo.reference_matching(name, id, force, current_id, log_message)?,
    })
}

fn parents_from_ids(repo: &Repository, mut parents: Vec<Oid>) -> Result<Vec<Commit>> {
    parents.sort();
    parents.dedup();
    parents.drain(..).map(|id| Ok(repo.find_commit(id)?)).collect()
}

struct Internals<'repo> {
    staged: TreeBuilder<'repo>,
    working: TreeBuilder<'repo>,
}

impl<'repo> Internals<'repo> {
    fn read(repo: &'repo Repository) -> Result<Self> {
        let shead = repo.find_reference(SHEAD_REF)?;
        let series_name = shead_series_name(&shead)?;
        let mut internals = Internals::read_series(repo, &series_name)?;
        internals.update_series(repo)?;
        Ok(internals)
    }

    fn read_series(repo: &'repo Repository, series_name: &str) -> Result<Self> {
        let committed_id = notfound_to_none(repo.refname_to_id(&format!("{}{}", SERIES_PREFIX, series_name)))?;
        let maybe_get_ref = |prefix: &str| -> Result<TreeBuilder<'repo>> {
            match notfound_to_none(repo.refname_to_id(&format!("{}{}", prefix, series_name)))?.or(committed_id) {
                Some(id) => {
                    let c = repo.find_commit(id)?;
                    let t = c.tree()?;
                    Ok(repo.treebuilder(Some(&t))?)
                }
                None => Ok(repo.treebuilder(None)?),
            }
        };
        Ok(Internals {
            staged: maybe_get_ref(STAGED_PREFIX)?,
            working: maybe_get_ref(WORKING_PREFIX)?,
        })
    }

    fn exists(repo: &'repo Repository, series_name: &str) -> Result<bool> {
        for prefix in [SERIES_PREFIX, STAGED_PREFIX, WORKING_PREFIX].iter() {
            let prefixed_name = format!("{}{}", prefix, series_name);
            if notfound_to_none(repo.refname_to_id(&prefixed_name))?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // Returns true if it had anything to copy.
    fn copy(repo: &'repo Repository, source: &str, dest: &str) -> Result<bool> {
        let mut copied_any = false;
        for prefix in [SERIES_PREFIX, STAGED_PREFIX, WORKING_PREFIX].iter() {
            let prefixed_source = format!("{}{}", prefix, source);
            if let Some(r) = notfound_to_none(repo.find_reference(&prefixed_source))? {
                let oid = r.target()
                    .ok_or(format!("Internal error: \"{}\" is a symbolic reference", prefixed_source))?;
                let prefixed_dest = format!("{}{}", prefix, dest);
                repo.reference(
                    &prefixed_dest,
                    oid,
                    false,
                    &format!("copied from {}", prefixed_source),
                )?;
                copied_any = true;
            }
        }
        Ok(copied_any)
    }

    // Returns true if it had anything to delete.
    fn delete(repo: &'repo Repository, series_name: &str) -> Result<bool> {
        let mut deleted_any = false;
        for prefix in [SERIES_PREFIX, STAGED_PREFIX, WORKING_PREFIX].iter() {
            let prefixed_name = format!("{}{}", prefix, series_name);
            if let Some(mut r) = notfound_to_none(repo.find_reference(&prefixed_name))? {
                r.delete()?;
                deleted_any = true;
            }
        }
        Ok(deleted_any)
    }

    fn update_series(&mut self, repo: &'repo Repository) -> Result<()> {
        let head_id = repo.refname_to_id("HEAD")?;
        self.working.insert("series", head_id, GIT_FILEMODE_COMMIT as i32)?;
        Ok(())
    }

    fn write(&self, repo: &'repo Repository) -> Result<()> {
        let config = repo.config()?;
        let author = get_signature(&config, "AUTHOR")?;
        let committer = get_signature(&config, "COMMITTER")?;

        let shead = repo.find_reference(SHEAD_REF)?;
        let series_name = shead_series_name(&shead)?;
        let maybe_commit = |prefix: &str, tb: &TreeBuilder| -> Result<()> {
            let tree_id = tb.write()?;
            let refname = format!("{}{}", prefix, series_name);
            let old_commit_id = notfound_to_none(repo.refname_to_id(&refname))?;
            if let Some(id) = old_commit_id {
                let c = repo.find_commit(id)?;
                if c.tree_id() == tree_id {
                    return Ok(());
                }
            }
            let tree = repo.find_tree(tree_id)?;
            let mut parents = Vec::new();
            // Include all commits from tree, to keep them reachable and fetchable. Include base,
            // because series might not have it as an ancestor; we don't enforce that until commit.
            for e in tree.iter() {
                if e.kind() == Some(ObjectType::Commit) {
                    parents.push(e.id());
                }
            }
            let parents = parents_from_ids(repo, parents)?;
            let parents_ref: Vec<&_> = parents.iter().collect();
            let commit_id = repo.commit(None, &author, &committer, &refname, &tree, &parents_ref)?;
            repo.reference_ensure_log(&refname)?;
            reference_matching_opt(
                repo,
                &refname,
                commit_id,
                true,
                old_commit_id,
                &format!("commit: {}", refname),
            )?;
            Ok(())
        };
        maybe_commit(STAGED_PREFIX, &self.staged)?;
        maybe_commit(WORKING_PREFIX, &self.working)?;
        Ok(())
    }
}

fn diff_empty(diff: &Diff) -> bool {
    diff.deltas().len() == 0
}

fn add(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let mut internals = Internals::read(repo)?;
    for file in m.values_of_os("change").unwrap() {
        match internals.working.get(file)? {
            Some(entry) => {
                internals.staged.insert(file, entry.id(), entry.filemode())?;
            }
            None => {
                if internals.staged.get(file)?.is_some() {
                    internals.staged.remove(file)?;
                }
            }
        }
    }
    internals.write(repo)
}

fn unadd(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let shead = repo.find_reference(SHEAD_REF)?;
    let started = {
        let shead_target = shead.symbolic_target().ok_or("SHEAD not a symbolic reference")?;
        notfound_to_none(repo.find_reference(shead_target))?.is_some()
    };

    let mut internals = Internals::read(repo)?;
    if started {
        let shead_commit = shead.peel_to_commit()?;
        let shead_tree = shead_commit.tree()?;

        for file in m.values_of("change").unwrap() {
            match shead_tree.get_name(file) {
                Some(entry) => {
                    internals.staged.insert(file, entry.id(), entry.filemode())?;
                }
                None => {
                    internals.staged.remove(file)?;
                }
            }
        }
    } else {
        for file in m.values_of("change").unwrap() {
            internals.staged.remove(file)?
        }
    }
    internals.write(repo)
}

fn shead_series_name(shead: &Reference) -> Result<String> {
    let shead_target = shead.symbolic_target().ok_or("SHEAD not a symbolic reference")?;
    if !shead_target.starts_with(SERIES_PREFIX) {
        return Err(format!("SHEAD does not start with {}", SERIES_PREFIX).into());
    }
    Ok(shead_target[SERIES_PREFIX.len()..].to_string())
}

fn series(out: &mut Output, repo: &Repository) -> Result<()> {
    let mut refs = Vec::new();
    for prefix in [SERIES_PREFIX, STAGED_PREFIX, WORKING_PREFIX].iter() {
        let l = prefix.len();
        for r in repo.references_glob(&[prefix, "*"].concat())?.names() {
            refs.push(r?[l..].to_string());
        }
    }
    let shead_target = if let Some(shead) = notfound_to_none(repo.find_reference(SHEAD_REF))? {
        Some(shead_series_name(&shead)?)
    } else {
        None
    };
    refs.extend(shead_target.clone().into_iter());
    refs.sort();
    refs.dedup();

    let config = repo.config()?.snapshot()?;
    out.auto_pager(&config, "branch", false)?;
    let color_current = out.get_color(&config, "branch", "current", "green")?;
    let color_plain = out.get_color(&config, "branch", "plain", "normal")?;
    for name in refs.iter() {
        let (star, color) = if Some(name) == shead_target.as_ref() {
            ('*', color_current)
        } else {
            (' ', color_plain)
        };
        let new = if notfound_to_none(repo.refname_to_id(&format!("{}{}", SERIES_PREFIX, name)))?.is_none() {
            " (new, no commits yet)"
        } else {
            ""
        };
        writeln!(out, "{} {}{}", star, color.paint(name as &str), new)?;
    }
    if refs.is_empty() {
        writeln!(out, "No series; use \"git series start <name>\" to start")?;
    }
    Ok(())
}

fn start(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let head = repo.head()?;
    let head_commit = head.peel_to_commit()?;
    let head_id = head_commit.as_object().id();

    let name = m.value_of("name").unwrap();
    if Internals::exists(repo, name)? {
        return Err(format!("Series {} already exists.\nUse checkout to resume working on an existing patch series.", name).into());
    }
    let prefixed_name = &[SERIES_PREFIX, name].concat();
    repo.reference_symbolic(
        SHEAD_REF,
        &prefixed_name,
        true,
        &format!("git series start {}", name),
    )?;

    let internals = Internals::read(repo)?;
    internals.write(repo)?;

    // git status parses this reflog string; the prefix must remain "checkout: moving from ".
    repo.reference(
        "HEAD",
        head_id,
        true,
        &format!("checkout: moving from {} to {} (git series start {})", head_id, head_id, name),
    )?;
    println!("HEAD is now detached at {}", commit_summarize(&repo, head_id)?);
    Ok(())
}

fn checkout_tree(repo: &Repository, treeish: &Object) -> Result<()> {
    let mut conflicts = Vec::new();
    let mut dirty = Vec::new();
    let result = {
        let mut opts = git2::build::CheckoutBuilder::new();
        opts.safe();
        opts.notify_on(git2::CheckoutNotificationType::CONFLICT | git2::CheckoutNotificationType::DIRTY);
        opts.notify(|t, path, _, _, _| {
            let path = path.unwrap().to_owned();
            if t == git2::CheckoutNotificationType::CONFLICT {
                conflicts.push(path);
            } else if t == git2::CheckoutNotificationType::DIRTY {
                dirty.push(path);
            }
            true
        });
        if atty::is(atty::Stream::Stdout) {
            opts.progress(|_, completed, total| {
                let total = total.to_string();
                print!("\rChecking out files: {1:0$}/{2}", total.len(), completed, total);
            });
        }
        repo.checkout_tree(treeish, Some(&mut opts))
    };
    match result {
        Err(ref e) if e.code() == git2::ErrorCode::Conflict => {
            let mut msg = String::new();
            writeln!(msg, "error: Your changes to the following files would be overwritten by checkout:").unwrap();
            for path in conflicts {
                writeln!(msg, "        {}", path.to_string_lossy()).unwrap();
            }
            writeln!(msg, "Please, commit your changes or stash them before you switch series.").unwrap();
            return Err(msg.into());
        }
        _ => result?,
    }
    println!();
    if !dirty.is_empty() {
        eprintln!("Files with changes unaffected by checkout:");
        for path in dirty {
            eprintln!("        {}", path.to_string_lossy());
        }
    }
    Ok(())
}

fn checkout(repo: &Repository, m: &ArgMatches) -> Result<()> {
    match repo.state() {
        git2::RepositoryState::Clean => (),
        s => return Err(format!("{:?} in progress; cannot checkout patch series", s).into()),
    }
    let name = m.value_of("name").unwrap();
    if !Internals::exists(repo, name)? {
        return Err(format!("Series {} does not exist.\nUse \"git series start <name>\" to start a new patch series.", name).into());
    }

    let internals = Internals::read_series(repo, name)?;
    let new_head_id = internals.working.get("series")?
        .ok_or(format!("Could not find \"series\" in \"{}\"", name))?
        .id();
    let new_head = repo.find_commit(new_head_id)?.into_object();

    checkout_tree(repo, &new_head)?;

    let head = repo.head()?;
    let head_commit = head.peel_to_commit()?;
    let head_id = head_commit.as_object().id();
    println!("Previous HEAD position was {}", commit_summarize(&repo, head_id)?);

    let prefixed_name = &[SERIES_PREFIX, name].concat();
    repo.reference_symbolic(
        SHEAD_REF,
        &prefixed_name,
        true,
        &format!("git series checkout {}", name),
    )?;
    internals.write(repo)?;

    // git status parses this reflog string; the prefix must remain "checkout: moving from ".
    repo.reference(
        "HEAD",
        new_head_id,
        true,
        &format!("checkout: moving from {} to {} (git series checkout {})", head_id, new_head_id, name),
    )?;
    println!("HEAD is now detached at {}", commit_summarize(&repo, new_head_id)?);

    Ok(())
}

fn base(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let mut internals = Internals::read(repo)?;

    let current_base_id = match internals.working.get("base")? {
        Some(entry) => entry.id(),
        _ => Oid::zero(),
    };

    if !m.is_present("delete") && !m.is_present("base") {
        if current_base_id.is_zero() {
            return Err("Patch series has no base set".into());
        } else {
            println!("{}", current_base_id);
            return Ok(());
        }
    }

    let new_base_id = if m.is_present("delete") {
        Oid::zero()
    } else {
        let base = m.value_of("base").unwrap();
        let base_object = repo.revparse_single(base)?;
        let base_commit = base_object.peel(ObjectType::Commit)?;
        let base_id = base_commit.id();
        let s_working_series = internals.working.get("series")?
            .ok_or("Could not find entry \"series\" in working vesion of current series")?;
        if base_id != s_working_series.id()
            && !repo.graph_descendant_of(s_working_series.id(), base_id)?
        {
            return Err(format!(
                "Cannot set base to {}: not an ancestor of the patch series {}",
                base,
                s_working_series.id(),
            ).into());
        }
        base_id
    };

    if current_base_id == new_base_id {
        println!("Base unchanged");
        return Ok(());
    }

    if !current_base_id.is_zero() {
        println!("Previous base was {}", commit_summarize(&repo, current_base_id)?);
    }

    if new_base_id.is_zero() {
        internals.working.remove("base")?;
        internals.write(repo)?;
        println!("Cleared patch series base");
    } else {
        internals.working.insert("base", new_base_id, GIT_FILEMODE_COMMIT as i32)?;
        internals.write(repo)?;
        println!("Set patch series base to {}", commit_summarize(&repo, new_base_id)?);
    }

    Ok(())
}

fn detach(repo: &Repository) -> Result<()> {
    match repo.find_reference(SHEAD_REF) {
        Ok(mut r) => r.delete()?,
        Err(_) => return Err("No current patch series to detach from.".into()),
    }
    Ok(())
}

fn delete(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let name = m.value_of("name").unwrap();
    if let Ok(shead) = repo.find_reference(SHEAD_REF) {
        let shead_target = shead_series_name(&shead)?;
        if shead_target == name {
            return Err(format!(
                "Cannot delete the current series \"{}\"; detach first.",
                name,
            ).into());
        }
    }
    if Internals::delete(repo, name)? == false {
        return Err(format!("Nothing to delete: series \"{}\" does not exist.", name).into());
    }
    Ok(())
}

fn do_diff(out: &mut Output, repo: &Repository) -> Result<()> {
    let internals = Internals::read(&repo)?;
    let config = repo.config()?.snapshot()?;
    out.auto_pager(&config, "diff", true)?;
    let diffcolors = DiffColors::new(out, &config)?;

    let working_tree = repo.find_tree(internals.working.write()?)?;
    let staged_tree = repo.find_tree(internals.staged.write()?)?;

    write_series_diff(out, repo, &diffcolors, Some(&staged_tree), Some(&working_tree))
}

fn get_editor(config: &Config) -> Result<OsString> {
    if let Some(e) = env::var_os("GIT_EDITOR") {
        return Ok(e);
    }
    if let Ok(e) = config.get_path("core.editor") {
        return Ok(e.into());
    }
    let terminal_is_dumb = match env::var_os("TERM") {
        None => true,
        Some(t) => t.as_os_str() == "dumb",
    };
    if !terminal_is_dumb {
        if let Some(e) = env::var_os("VISUAL") {
            return Ok(e);
        }
    }
    if let Some(e) = env::var_os("EDITOR") {
        return Ok(e);
    }
    if terminal_is_dumb {
        return Err("TERM unset or \"dumb\" but EDITOR unset".into());
    }
    Ok("vi".into())
}

// Get the pager to use; with for_cmd set, get the pager for use by the
// specified git command.  If get_pager returns None, don't use a pager.
fn get_pager(config: &Config, for_cmd: &str, default: bool) -> Option<OsString> {
    if !atty::is(atty::Stream::Stdout) {
        return None;
    }
    // pager.cmd can contain a boolean (if false, force no pager) or a
    // command-specific pager; only treat it as a command if it doesn't parse
    // as a boolean.
    let maybe_pager = config.get_path(&format!("pager.{}", for_cmd)).ok();
    let (cmd_want_pager, cmd_pager) = maybe_pager.map_or((default, None), |p|
            if let Ok(b) = Config::parse_bool(&p) {
                (b, None)
            } else {
                (true, Some(p))
            }
        );
    if !cmd_want_pager {
        return None;
    }
    let pager = if let Some(e) = env::var_os("GIT_PAGER") {
        Some(e)
    } else if let Some(p) = cmd_pager {
        Some(p.into())
    } else if let Ok(e) = config.get_path("core.pager") {
        Some(e.into())
    } else if let Some(e) = env::var_os("PAGER") {
        Some(e)
    } else {
        Some("less".into())
    };
    pager.and_then(|p| if p.is_empty() || p == "cat" { None } else { Some(p) })
}

/// Construct a Command, using the shell if the command contains shell metachars
fn cmd_maybe_shell<S: AsRef<OsStr>>(program: S, args: bool) -> Command {
    if program.as_ref().to_string_lossy().contains(|c| SHELL_METACHARS.contains(c)) {
        let mut cmd = Command::new("sh");
        cmd.arg("-c");
        if args {
            let mut program_with_args = program.as_ref().to_os_string();
            program_with_args.push(" \"$@\"");
            cmd.arg(program_with_args).arg(program);
        } else {
            cmd.arg(program);
        }
        cmd
    } else {
        Command::new(program)
    }
}

fn run_editor<S: AsRef<OsStr>>(config: &Config, filename: S) -> Result<()> {
    let editor = get_editor(&config)?;
    let editor_status = cmd_maybe_shell(editor, true).arg(&filename).status()?;
    if !editor_status.success() {
        return Err(format!("Editor exited with status {}", editor_status).into());
    }
    Ok(())
}

struct Output {
    pager: Option<std::process::Child>,
    include_stderr: bool,
}

impl Output {
    fn new() -> Self {
        Output { pager: None, include_stderr: false }
    }

    fn auto_pager(&mut self, config: &Config, for_cmd: &str, default: bool) -> Result<()> {
        if let Some(pager) = get_pager(config, for_cmd, default) {
            let mut cmd = cmd_maybe_shell(pager, false);
            cmd.stdin(std::process::Stdio::piped());
            if env::var_os("LESS").is_none() {
                cmd.env("LESS", "FRX");
            }
            if env::var_os("LV").is_none() {
                cmd.env("LV", "-c");
            }
            let child = cmd.spawn()?;
            self.pager = Some(child);
            self.include_stderr = atty::is(atty::Stream::Stderr);
        }
        Ok(())
    }

    // Get a color to write text with, taking git configuration into account.
    //
    // config: the configuration to determine the color from.
    // command: the git command to act like.
    // slot: the color "slot" of that git command to act like.
    // default: the color to use if not configured.
    fn get_color(
        &self,
        config: &Config,
        command: &str,
        slot: &str,
        default: &str,
    ) -> Result<Style> {
        if !cfg!(unix) {
            return Ok(Style::new());
        }
        let color_ui = notfound_to_none(config.get_str("color.ui"))?.unwrap_or("auto");
        let color_cmd = notfound_to_none(config.get_str(&format!("color.{}", command)))?.unwrap_or(color_ui);
        if color_cmd == "never" || Config::parse_bool(color_cmd) == Ok(false) {
            return Ok(Style::new());
        }
        if self.pager.is_some() {
            let color_pager = notfound_to_none(config.get_bool("color.pager"))?.unwrap_or(true);
            if !color_pager {
                return Ok(Style::new());
            }
        } else if !atty::is(atty::Stream::Stdout) {
            return Ok(Style::new());
        }
        let cfg = format!("color.{}.{}", command, slot);
        let color = notfound_to_none(config.get_str(&cfg))?.unwrap_or(default);
        colorparse::parse(color).map_err(|e| format!("Error parsing {}: {}", cfg, e).into())
    }

    fn write_err(&mut self, msg: &str) {
        if self.include_stderr {
            if write!(self, "{}", msg).is_err() {
                eprint!("{}", msg);
            }
        } else {
            eprint!("{}", msg);
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.pager {
            let status = child.wait().unwrap();
            if !status.success() {
                eprintln!("Pager exited with status {}", status);
            }
        }
    }
}

impl IoWrite for Output {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.pager {
            Some(ref mut child) => child.stdin.as_mut().unwrap().write(buf),
            None => std::io::stdout().write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.pager {
            Some(ref mut child) => child.stdin.as_mut().unwrap().flush(),
            None => std::io::stdout().flush(),
        }
    }
}

fn get_signature(config: &Config, which: &str) -> Result<git2::Signature<'static>> {
    let name_var = ["GIT_", which, "_NAME"].concat();
    let email_var = ["GIT_", which, "_EMAIL"].concat();
    let which_lc = which.to_lowercase();
    let name = env::var(&name_var)
        .or_else(|_| config.get_string("user.name"))
        .or_else(|_| Err(format!(
            "Could not determine {} name: checked ${} and user.name in git config",
            which_lc, name_var,
        )))?;
    let email = env::var(&email_var)
        .or_else(|_| config.get_string("user.email"))
        .or_else(|_| env::var("EMAIL"))
        .or_else(|_| Err(format!(
            "Could not determine {} email: checked ${}, user.email in git config, and $EMAIL",
            which_lc, email_var,
        )))?;
    Ok(git2::Signature::now(&name, &email)?)
}

fn commit_status(
    out: &mut Output,
    repo: &Repository,
    m: &ArgMatches,
    do_status: bool,
) -> Result<()> {
    let config = repo.config()?.snapshot()?;
    let shead = match notfound_to_none(repo.find_reference(SHEAD_REF))? {
        None => {
            println!("No series; use \"git series start <name>\" to start");
            return Ok(());
        }
        Some(result) => result,
    };
    let series_name = shead_series_name(&shead)?;

    if do_status {
        out.auto_pager(&config, "status", false)?;
    }
    let get_color = |out: &Output, color: &str, default: &str| {
        if do_status {
            out.get_color(&config, "status", color, default)
        } else {
            Ok(Style::new())
        }
    };
    let color_normal = Style::new();
    let color_header = get_color(out, "header", "normal")?;
    let color_updated = get_color(out, "updated", "green")?;
    let color_changed = get_color(out, "changed", "red")?;

    let write_status = |
        status: &mut Vec<ansi_term::ANSIString>,
        diff: &Diff,
        heading: &str,
        color: &Style,
        show_hints: bool,
        hints: &[&str],
    | -> Result<bool> {
        let mut changes = false;

        diff.foreach(&mut |delta, _| {
            if !changes {
                changes = true;
                status.push(color_header.paint(format!("{}\n", heading.to_string())));
                if show_hints {
                    for hint in hints {
                        status.push(color_header.paint(format!("  ({})\n", hint)));
                    }
                }
                status.push(color_normal.paint("\n"));
            }
            status.push(color_normal.paint("        "));
            status.push(color.paint(format!(
                "{:?}:   {}\n",
                delta.status(),
                delta.old_file().path().unwrap().to_str().unwrap(),
            )));
            true
        }, None, None, None)?;

        if changes {
            status.push(color_normal.paint("\n"));
        }

        Ok(changes)
    };

    let mut status = Vec::new();
    status.push(color_header.paint(format!("On series {}\n", series_name)));

    let mut internals = Internals::read(repo)?;
    let working_tree = repo.find_tree(internals.working.write()?)?;
    let staged_tree = repo.find_tree(internals.staged.write()?)?;

    let shead_commit = match notfound_to_none(shead.resolve())? {
        Some(r) => Some(r.peel_to_commit()?),
        None => {
            status.push(color_header.paint("\nInitial series commit\n"));
            None
        }
    };
    let shead_tree = match shead_commit {
        Some(ref c) => Some(c.tree()?),
        None => None,
    };

    let commit_all = m.is_present("all");

    let (changes, tree) = if commit_all {
        let diff = repo.diff_tree_to_tree(shead_tree.as_ref(), Some(&working_tree), None)?;
        let changes = write_status(
            &mut status,
            &diff,
            "Changes to be committed:",
            &color_normal,
            false,
            &[],
        )?;
        if !changes {
            status.push(color_normal.paint("nothing to commit; series unchanged\n"));
        }
        (changes, working_tree)
    } else {
        let diff = repo.diff_tree_to_tree(shead_tree.as_ref(), Some(&staged_tree), None)?;
        let changes_to_be_committed = write_status(
            &mut status,
            &diff,
            "Changes to be committed:",
            &color_updated,
            do_status,
            &[
                "use \"git series commit\" to commit",
                "use \"git series unadd <file>...\" to undo add",
            ],
        )?;

        let diff_not_staged = repo.diff_tree_to_tree(Some(&staged_tree), Some(&working_tree), None)?;
        let changes_not_staged = write_status(
            &mut status,
            &diff_not_staged,
            "Changes not staged for commit:",
            &color_changed,
            do_status,
            &["use \"git series add <file>...\" to update what will be committed"],
        )?;

        if !changes_to_be_committed {
            if changes_not_staged {
                status.push(color_normal.paint("no changes added to commit (use \"git series add\" or \"git series commit -a\")\n"));
            } else {
                status.push(color_normal.paint("nothing to commit; series unchanged\n"));
            }
        }

        (changes_to_be_committed, staged_tree)
    };

    let status = ansi_term::ANSIStrings(&status).to_string();
    if do_status {
        write!(out, "{}", status)?;
        return Ok(());
    } else if !changes {
        return Err(status.into());
    }

    // Check that the commit includes the series
    let series_id = match tree.get_name("series") {
        None => {
            return Err(concat!(
                "Cannot commit: initial commit must include \"series\"\n",
                "Use \"git series add series\" or \"git series commit -a\"",
            ).into());
        }
        Some(series) => series.id(),
    };

    // Check that the base is still an ancestor of the series
    if let Some(base) = tree.get_name("base") {
        if base.id() != series_id && !repo.graph_descendant_of(series_id, base.id())? {
            let (base_short_id, base_summary) = commit_summarize_components(&repo, base.id())?;
            let (series_short_id, series_summary) = commit_summarize_components(&repo, series_id)?;
            return Err(format!(
                concat!(
                    "Cannot commit: base {} is not an ancestor of patch series {}\n",
                    "base   {} {}\n",
                    "series {} {}"
                ),
                base_short_id, series_short_id,
                base_short_id, base_summary,
                series_short_id, series_summary,
            ).into());
        }
    }

    let msg = match m.value_of("m") {
        Some(s) => s.to_string(),
        None => {
            let filename = repo.path().join("SCOMMIT_EDITMSG");
            let mut file = File::create(&filename)?;
            write!(file, "{}", COMMIT_MESSAGE_COMMENT)?;
            for line in status.lines() {
                if line.is_empty() {
                    writeln!(file, "#")?;
                } else {
                    writeln!(file, "# {}", line)?;
                }
            }
            if m.is_present("verbose") {
                writeln!(file, "{}\n{}", SCISSOR_LINE, SCISSOR_COMMENT)?;
                write_series_diff(
                    &mut file,
                    repo,
                    &DiffColors::plain(),
                    shead_tree.as_ref(),
                    Some(&tree),
                )?;
            }
            drop(file);
            run_editor(&config, &filename)?;
            let mut file = File::open(&filename)?;
            let mut msg = String::new();
            file.read_to_string(&mut msg)?;
            if let Some(scissor_index) = msg.find(SCISSOR_LINE) {
                msg.truncate(scissor_index);
            }
            git2::message_prettify(msg, git2::DEFAULT_COMMENT_CHAR)?
        }
    };
    if msg.is_empty() {
        return Err("Aborting series commit due to empty commit message.".into());
    }

    let author = get_signature(&config, "AUTHOR")?;
    let committer = get_signature(&config, "COMMITTER")?;
    let mut parents: Vec<Oid> = Vec::new();
    // Include all commits from tree, to keep them reachable and fetchable.
    for e in tree.iter() {
        if e.kind() == Some(ObjectType::Commit) && e.name().unwrap() != "base" {
            parents.push(e.id())
        }
    }
    let parents = parents_from_ids(repo, parents)?;
    let parents_ref: Vec<&_> = shead_commit.iter().chain(parents.iter()).collect();
    let new_commit_oid = repo.commit(Some(SHEAD_REF), &author, &committer, &msg, &tree, &parents_ref)?;

    if commit_all {
        internals.staged = repo.treebuilder(Some(&tree))?;
        internals.write(repo)?;
    }

    let (new_commit_short_id, new_commit_summary) = commit_summarize_components(&repo, new_commit_oid)?;
    writeln!(out, "[{} {}] {}", series_name, new_commit_short_id, new_commit_summary)?;

    Ok(())
}

fn cover(repo: &Repository, m: &ArgMatches) -> Result<()> {
    let mut internals = Internals::read(repo)?;

    let (working_cover_id, working_cover_content) = match internals.working.get("cover")? {
        None => (Oid::zero(), String::new()),
        Some(entry) => (entry.id(), std::str::from_utf8(repo.find_blob(entry.id())?.content())?.to_string()),
    };

    if m.is_present("delete") {
        if working_cover_id.is_zero() {
            return Err("No cover to delete".into());
        }
        internals.working.remove("cover")?;
        internals.write(repo)?;
        println!("Deleted cover letter");
        return Ok(());
    }

    let filename = repo.path().join("COVER_EDITMSG");
    let mut file = File::create(&filename)?;
    if working_cover_content.is_empty() {
        write!(file, "{}", COVER_LETTER_COMMENT)?;
    } else {
        write!(file, "{}", working_cover_content)?;
    }
    drop(file);
    let config = repo.config()?;
    run_editor(&config, &filename)?;
    let mut file = File::open(&filename)?;
    let mut msg = String::new();
    file.read_to_string(&mut msg)?;
    let msg = git2::message_prettify(msg, git2::DEFAULT_COMMENT_CHAR)?;
    if msg.is_empty() {
        return Err("Empty cover letter; not changing.\n(To delete the cover letter, use \"git series cover -d\".)".into());
    }

    let new_cover_id = repo.blob(msg.as_bytes())?;
    if new_cover_id == working_cover_id {
        println!("Cover letter unchanged");
    } else {
        internals.working.insert("cover", new_cover_id, GIT_FILEMODE_BLOB as i32)?;
        internals.write(repo)?;
        println!("Updated cover letter");
    }

    Ok(())
}

fn cp_mv(repo: &Repository, m: &ArgMatches, mv: bool) -> Result<()> {
    let shead_target = if let Some(shead) = notfound_to_none(repo.find_reference(SHEAD_REF))? {
        Some(shead_series_name(&shead)?)
    } else {
        None
    };
    let mut source_dest = m.values_of("source_dest").unwrap();
    let dest = source_dest.next_back().unwrap();
    let (update_shead, source) = match source_dest.next_back().map(String::from) {
        Some(name) => (shead_target.as_ref() == Some(&name), name),
        None => (true, shead_target.ok_or("No current series")?),
    };

    if Internals::exists(&repo, dest)? {
        return Err(format!("The destination series \"{}\" already exists", dest).into());
    }
    if !Internals::copy(&repo, &source, &dest)? {
        return Err(format!("The source series \"{}\" does not exist", source).into());
    }

    if mv {
        if update_shead {
            let prefixed_dest = &[SERIES_PREFIX, dest].concat();
            repo.reference_symbolic(
                SHEAD_REF,
                &prefixed_dest,
                true,
                &format!("git series mv {} {}", source, dest),
            )?;
        }
        Internals::delete(&repo, &source)?;
    }

    Ok(())
}

fn date_822(t: git2::Time) -> String {
    let offset = chrono::offset::fixed::FixedOffset::east(t.offset_minutes() * 60);
    let datetime = offset.timestamp(t.seconds(), 0);
    datetime.to_rfc2822()
}

fn shortlog(commits: &mut [Commit]) -> String {
    let mut s = String::new();
    let mut author_map = std::collections::HashMap::new();

    for commit in commits {
        let author = commit.author().name().unwrap().to_string();
        author_map.entry(author).or_insert_with(Vec::new)
            .push(commit.summary().unwrap().to_string());
    }

    let mut authors: Vec<_> = author_map.keys().collect();
    authors.sort();
    let mut first = true;
    for author in authors {
        if first {
            first = false;
        } else {
            writeln!(s).unwrap();
        }
        let summaries = author_map.get(author).unwrap();
        writeln!(s, "{} ({}):", author, summaries.len()).unwrap();
        for summary in summaries {
            writeln!(s, "  {}", summary).unwrap();
        }
    }

    s
}

fn sanitize_summary(summary: &str) -> String {
    let mut s = String::with_capacity(summary.len());
    let mut prev_dot = false;
    let mut need_space = false;
    for c in summary.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            if need_space {
                s.push('-');
                need_space = false;
            }
            if !(prev_dot && c == '.') {
                s.push(c);
            }
        } else {
            if !s.is_empty() {
                need_space = true;
            }
        }
        prev_dot = c == '.';
    }
    let end = s.trim_end_matches(|c| c == '.' || c == '-').len();
    s.truncate(end);
    s
}

#[test]
fn test_sanitize_summary() {
    let tests = vec![
        ("", ""),
        ("!!!!!", ""),
        ("Test", "Test"),
        ("Test case", "Test-case"),
        ("Test    case", "Test-case"),
        ("    Test    case    ", "Test-case"),
        ("...Test...case...", ".Test.case"),
        ("...Test...case.!!", ".Test.case"),
        (".!.Test.!.case.!.", ".-.Test.-.case"),
    ];
    for (summary, sanitized) in tests {
        assert_eq!(sanitize_summary(summary), sanitized.to_string());
    }
}

fn split_message(message: &str) -> (&str, &str) {
    let mut iter = message.splitn(2, '\n');
    let subject = iter.next().unwrap().trim_end();
    let body = iter.next().map(|s| s.trim_start()).unwrap_or("");
    (subject, body)
}

struct DiffColors {
    commit: Style,
    meta: Style,
    frag: Style,
    func: Style,
    context: Style,
    old: Style,
    new: Style,
    series_old: Style,
    series_new: Style,
}

impl DiffColors {
    fn plain() -> Self {
        DiffColors {
            commit: Style::new(),
            meta: Style::new(),
            frag: Style::new(),
            func: Style::new(),
            context: Style::new(),
            old: Style::new(),
            new: Style::new(),
            series_old: Style::new(),
            series_new: Style::new(),
        }
    }

    fn new(out: &Output, config: &Config) -> Result<Self> {
        let old = out.get_color(&config, "diff", "old", "red")?;
        let new = out.get_color(&config, "diff", "new", "green")?;
        Ok(DiffColors {
            commit: out.get_color(&config, "diff", "commit", "yellow")?,
            meta: out.get_color(&config, "diff", "meta", "bold")?,
            frag: out.get_color(&config, "diff", "frag", "cyan")?,
            func: out.get_color(&config, "diff", "func", "normal")?,
            context: out.get_color(&config, "diff", "context", "normal")?,
            old,
            new,
            series_old: old.reverse(),
            series_new: new.reverse(),
        })
    }
}

fn diffstat(diff: &Diff) -> Result<String> {
    let stats = diff.stats()?;
    let stats_buf = stats.to_buf(git2::DiffStatsFormat::FULL | git2::DiffStatsFormat::INCLUDE_SUMMARY, 72)?;
    Ok(stats_buf.as_str().unwrap().to_string())
}

fn write_diff<W: IoWrite>(
    f: &mut W,
    colors: &DiffColors,
    diff: &Diff,
    simplify: bool,
) -> Result<usize> {
    let mut err = Ok(());
    let mut lines = 0;
    let normal = Style::new();
    diff.print(git2::DiffFormat::Patch, |_, _, l| {
        err = || -> Result<()> {
            let o = l.origin();
            let style = match o {
                '-' | '<' => colors.old,
                '+' | '>' => colors.new,
                _ if simplify => normal,
                ' ' | '=' => colors.context,
                'F' => colors.meta,
                'H' => colors.frag,
                _ => normal,
            };
            let obyte = [o as u8];
            let mut v = Vec::new();
            if o == '+' || o == '-' || o == ' ' {
                v.push(style.paint(&obyte[..]));
            }
            if simplify {
                if o == 'H' {
                    v.push(normal.paint("@@\n".as_bytes()));
                    lines += 1;
                } else if o == 'F' {
                    for line in l.content().split(|c| *c == b'\n') {
                        if !line.is_empty()
                            && !line.starts_with(b"diff --git")
                            && !line.starts_with(b"index ")
                        {
                            v.push(normal.paint(line.to_owned()));
                            v.push(normal.paint("\n".as_bytes()));
                            lines += 1;
                        }
                    }
                } else {
                    v.push(style.paint(l.content()));
                    lines += 1;
                }
            } else if o == 'H' {
                // Split frag and func
                let line = l.content();
                let at = &|&(_, &c): &(usize, &u8)| c == b'@';
                let not_at = &|&(_, &c): &(usize, &u8)| c != b'@';
                match line
                    .iter()
                    .enumerate()
                    .skip_while(at)
                    .skip_while(not_at)
                    .skip_while(at)
                    .nth(1)
                    .unwrap_or((0, &b'\n'))
                {
                    (_, &c) if c == b'\n' => v.push(style.paint(&line[..line.len() - 1])),
                    (pos, _) => {
                        v.push(style.paint(&line[..pos - 1]));
                        v.push(normal.paint(" ".as_bytes()));
                        v.push(colors.func.paint(&line[pos..line.len() - 1]));
                    }
                }
                v.push(normal.paint("\n".as_bytes()));
            } else {
                // The less pager resets ANSI colors at each newline, so emit colors separately for
                // each line.
                for (n, line) in l.content().split(|c| *c == b'\n').enumerate() {
                    if n != 0 {
                        v.push(normal.paint("\n".as_bytes()));
                    }
                    if !line.is_empty() {
                        v.push(style.paint(line));
                    }
                }
            }
            ansi_term::ANSIByteStrings(&v).write_to(f)?;
            Ok(())
        }();
        err.is_ok()
    })?;
    err?;
    Ok(lines)
}

fn get_commits(repo: &Repository, base: Oid, series: Oid) -> Result<Vec<Commit>> {
    let mut revwalk = repo.revwalk()?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE);
    revwalk.push(series)?;
    revwalk.hide(base)?;
    revwalk.map(|c| {
        let id = c?;
        let commit = repo.find_commit(id)?;
        Ok(commit)
    }).collect()
}

fn write_commit_range_diff<W: IoWrite>(
    out: &mut W,
    repo: &Repository,
    colors: &DiffColors,
    (base1, series1): (Oid, Oid),
    (base2, series2): (Oid, Oid),
) -> Result<()> {
    let mut commits1 = get_commits(repo, base1, series1)?;
    let mut commits2 = get_commits(repo, base2, series2)?;
    for commit in commits1.iter().chain(commits2.iter()) {
        if commit.parent_ids().count() > 1 {
            writeln!(out, "(Diffs of series with merge commits ({}) not yet supported)", commit.id())?;
            return Ok(());
        }
    }
    let ncommon = commits1.iter().zip(commits2.iter())
        .take_while(|&(ref c1, ref c2)| c1.id() == c2.id())
        .count();
    drop(commits1.drain(..ncommon));
    drop(commits2.drain(..ncommon));
    let ncommits1 = commits1.len();
    let ncommits2 = commits2.len();
    let n = ncommits1 + ncommits2;
    if n == 0 {
        return Ok(());
    }
    let commit_text = &|commit: &Commit| {
        let parent = commit.parent(0)?;
        let author = commit.author();
        let diff = repo.diff_tree_to_tree(
            Some(&parent.tree().unwrap()),
            Some(&commit.tree().unwrap()),
            None,
        )?;
        let mut v = Vec::new();
        v.write_all(b"From: ")?;
        v.write_all(author.name_bytes())?;
        v.write_all(b" <")?;
        v.write_all(author.email_bytes())?;
        v.write_all(b">\n\n")?;
        v.write_all(commit.message_bytes())?;
        v.write_all(b"\n")?;
        let lines = write_diff(&mut v, colors, &diff, true)?;
        Ok((v, lines))
    };
    let texts1: Vec<_> = commits1.iter().map(commit_text).collect::<Result<_>>()?;
    let texts2: Vec<_> = commits2.iter().map(commit_text).collect::<Result<_>>()?;

    let mut weights = Vec::with_capacity(n * n);
    for i1 in 0..ncommits1 {
        for i2 in 0..ncommits2 {
            let patch = git2::Patch::from_buffers(&texts1[i1].0, None, &texts2[i2].0, None, None)?;
            let (_, additions, deletions) = patch.line_stats()?;
            weights.push(additions + deletions);
        }
        let w = texts1[i1].1 / 2;
        for _ in ncommits2..n {
            weights.push(w);
        }
    }
    for _ in ncommits1..n {
        for i2 in 0..ncommits2 {
            weights.push(texts2[i2].1 / 2);
        }
        for _ in ncommits2..n {
            weights.push(0);
        }
    }
    let mut weight_matrix = munkres::WeightMatrix::from_row_vec(n, weights);
    let result = munkres::solve_assignment(&mut weight_matrix)?;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum CommitState { Unhandled, Handled, Deleted }
    let mut commits2_from1: Vec<_> = std::iter::repeat(None).take(ncommits2).collect();
    let mut commits1_state: Vec<_> = std::iter::repeat(CommitState::Unhandled).take(ncommits1).collect();
    let mut commit_pairs = Vec::with_capacity(n);
    for munkres::Position { row: i1, column: i2 } in result {
        if i1 < ncommits1 {
            if i2 < ncommits2 {
                commits2_from1[i2] = Some(i1);
            } else {
                commits1_state[i1] = CommitState::Deleted;
            }
        }
    }

    // Show matching or new commits sorted by the new commit order. Show deleted commits after
    // showing all of their prerequisite commits.
    let mut commits1_state_index = 0;
    for (i2, opt_i1) in commits2_from1.iter().enumerate() {
        while commits1_state_index < ncommits1 {
            match commits1_state[commits1_state_index] {
                CommitState::Unhandled => { break }
                CommitState::Handled => {}
                CommitState::Deleted => {
                    commit_pairs.push((Some(commits1_state_index), None));
                }
            }
            commits1_state_index += 1;
        }
        if let &Some(i1) = opt_i1 {
            commit_pairs.push((Some(i1), Some(i2)));
            commits1_state[i1] = CommitState::Handled;
        } else {
            commit_pairs.push((None, Some(i2)));
        }
    }
    for i1 in commits1_state_index..ncommits1 {
        if commits1_state[i1] == CommitState::Deleted {
            commit_pairs.push((Some(i1), None));
        }
    }

    let normal = Style::new();
    let nl = |v: &mut Vec<_>| { v.push(normal.paint("\n".as_bytes())); };
    let mut v = Vec::new();
    v.push(colors.meta.paint("diff --series".as_bytes()));
    nl(&mut v);

    let offset = ncommon + 1;
    let nwidth = max(ncommits1 + offset, ncommits2 + offset).to_string().len();
    let commits1_summaries: Vec<_> = commits1.iter_mut().map(commit_obj_summarize_components).collect::<Result<_>>()?;
    let commits2_summaries: Vec<_> = commits2.iter_mut().map(commit_obj_summarize_components).collect::<Result<_>>()?;
    let idwidth = commits1_summaries.iter().chain(commits2_summaries.iter())
        .map(|&(ref short_id, _)| short_id.len())
        .max().unwrap();
    for commit_pair in commit_pairs {
        match commit_pair {
            (None, None) => unreachable!(),
            (Some(i1), None) => {
                let (ref c1_short_id, ref c1_summary) = commits1_summaries[i1];
                v.push(colors.old.paint(format!(
                    "{:nwidth$}: {:idwidth$} < {:-<nwidth$}: {:-<idwidth$} {}",
                    i1 + offset, c1_short_id, "", "", c1_summary, nwidth=nwidth, idwidth=idwidth,
                ).as_bytes().to_owned()));
                nl(&mut v);
            }
            (None, Some(i2)) => {
                let (ref c2_short_id, ref c2_summary) = commits2_summaries[i2];
                v.push(colors.new.paint(format!(
                    "{:-<nwidth$}: {:-<idwidth$} > {:nwidth$}: {:idwidth$} {}",
                    "", "", i2 + offset, c2_short_id, c2_summary, nwidth=nwidth, idwidth=idwidth,
                ).as_bytes().to_owned()));
                nl(&mut v);
            }
            (Some(i1), Some(i2)) => {
                let mut patch = git2::Patch::from_buffers(&texts1[i1].0, None, &texts2[i2].0, None, None)?;
                let (old, ch, new) = if let Delta::Unmodified = patch.delta().status() {
                    (colors.commit, '=', colors.commit)
                } else {
                    (colors.series_old, '!', colors.series_new)
                };
                let (ref c1_short_id, _) = commits1_summaries[i1];
                let (ref c2_short_id, ref c2_summary) = commits2_summaries[i2];
                v.push(old.paint(format!("{:nwidth$}: {:idwidth$}", i1 + offset, c1_short_id, nwidth=nwidth, idwidth=idwidth).as_bytes().to_owned()));
                v.push(colors.commit.paint(format!(" {} ", ch).as_bytes().to_owned()));
                v.push(new.paint(format!("{:nwidth$}: {:idwidth$}", i2 + offset, c2_short_id, nwidth=nwidth, idwidth=idwidth).as_bytes().to_owned()));
                v.push(colors.commit.paint(format!(" {}", c2_summary).as_bytes().to_owned()));
                nl(&mut v);
                patch.print(&mut |_, _, l| {
                    let o = l.origin();
                    let style = match o {
                        '-' | '<' => old,
                        '+' | '>' => new,
                        _ => normal,
                    };
                    if o == '+' || o == '-' || o == ' ' {
                        v.push(style.paint(vec![o as u8]));
                    }
                    let style = if o == 'H' { colors.frag } else { normal };
                    if o != 'F' {
                        v.push(style.paint(l.content().to_owned()));
                    }
                    true
                })?;
            }
        }
    }

    ansi_term::ANSIByteStrings(&v).write_to(out)?;
    Ok(())
}

fn write_series_diff<W: IoWrite>(
    out: &mut W,
    repo: &Repository,
    colors: &DiffColors,
    tree1: Option<&Tree>,
    tree2: Option<&Tree>,
) -> Result<()> {
    let diff = repo.diff_tree_to_tree(tree1, tree2, None)?;
    write_diff(out, colors, &diff, false)?;

    let base1 = tree1.and_then(|t| t.get_name("base"));
    let series1 = tree1.and_then(|t| t.get_name("series"));
    let base2 = tree2.and_then(|t| t.get_name("base"));
    let series2 = tree2.and_then(|t| t.get_name("series"));

    if let (Some(base1), Some(series1), Some(base2), Some(series2)) = (base1, series1, base2, series2) {
        write_commit_range_diff(
            out,
            repo,
            colors,
            (base1.id(), series1.id()),
            (base2.id(), series2.id()),
        )?;
    } else {
        writeln!(out, "Can't diff series: both versions must have base and series to diff")?;
    }

    Ok(())
}

fn mail_signature() -> String {
    format!("-- \ngit-series {}", clap::crate_version!())
}

fn ensure_space(s: &str) -> &'static str {
    if s.is_empty() || s.ends_with(' ') {
        ""
    } else {
        " "
    }
}

fn ensure_nl(s: &str) -> &'static str {
    if !s.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn format(out: &mut Output, repo: &Repository, m: &ArgMatches) -> Result<()> {
    let config = repo.config()?.snapshot()?;
    let to_stdout = m.is_present("stdout");
    let no_from = m.is_present("no-from");

    let shead_commit = repo.find_reference(SHEAD_REF)?.resolve()?.peel_to_commit()?;
    let stree = shead_commit.tree()?;

    let series = stree.get_name("series")
        .ok_or("Internal error: series did not contain \"series\"")?;
    let base = stree.get_name("base")
        .ok_or("Cannot format series; no base set.\nUse \"git series base\" to set base.")?;

    let mut revwalk = repo.revwalk()?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE);
    revwalk.push(series.id())?;
    revwalk.hide(base.id())?;
    let mut commits: Vec<Commit> = revwalk.map(|c| {
        let id = c?;
        let commit = repo.find_commit(id)?;
        if commit.parent_ids().count() > 1 {
            return Err(format!(
                "Error: cannot format merge commit as patch:\n{}",
                commit_summarize(repo, id)?,
            ).into());
        }
        Ok(commit)
    }).collect::<Result<_>>()?;
    if commits.is_empty() {
        return Err("No patches to format; series and base identical.".into());
    }

    let committer = get_signature(&config, "COMMITTER")?;
    let committer_name = committer.name().unwrap();
    let committer_email = committer.email().unwrap();
    let message_id_suffix = format!(
        "{}.git-series.{}",
        committer.when().seconds(),
        committer_email,
    );

    let cover_entry = stree.get_name("cover");
    let mut in_reply_to_message_id = m.value_of("in-reply-to")
        .map(|v| format!(
            "{}{}{}",
            if v.starts_with('<') { "" } else { "<" },
            v,
            if v.ends_with('>') { "" } else { ">" },
        ));

    let version = m.value_of("reroll-count");
    let subject_prefix = if m.is_present("rfc") {
        "RFC PATCH"
    } else {
        m.value_of("subject-prefix").unwrap_or("PATCH")
    };
    let subject_patch = version.map_or(
        subject_prefix.to_string(),
        |n| format!("{}{}v{}", subject_prefix, ensure_space(&subject_prefix), n),
    );
    let file_prefix = version.map_or("".to_string(), |n| format!("v{}-", n));

    let num_width = commits.len().to_string().len();

    let signature = mail_signature();

    if to_stdout {
        out.auto_pager(&config, "format-patch", true)?;
    }
    let diffcolors = if to_stdout {
        DiffColors::new(out, &config)?
    } else {
        DiffColors::plain()
    };
    let mut out: Box<dyn IoWrite> = if to_stdout {
        Box::new(out)
    } else {
        Box::new(std::io::stdout())
    };
    let patch_file = |name: &str| -> Result<Box<dyn IoWrite>> {
        let name = format!("{}{}", file_prefix, name);
        println!("{}", name);
        Ok(Box::new(File::create(name)?))
    };

    if let Some(ref entry) = cover_entry {
        let cover_blob = repo.find_blob(entry.id())?;
        let content = std::str::from_utf8(cover_blob.content())?.to_string();
        let (subject, body) = split_message(&content);

        let series_tree = repo.find_commit(series.id())?.tree().unwrap();
        let base_tree = repo.find_commit(base.id())?.tree().unwrap();
        let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&series_tree), None)?;
        let stats = diffstat(&diff)?;

        if !to_stdout {
            out = patch_file("0000-cover-letter.patch")?;
        }
        writeln!(out, "From {} Mon Sep 17 00:00:00 2001", shead_commit.id())?;
        let cover_message_id = format!("<cover.{}.{}>", shead_commit.id(), message_id_suffix);
        writeln!(out, "Message-Id: {}", cover_message_id)?;
        if let Some(ref message_id) = in_reply_to_message_id {
            writeln!(out, "In-Reply-To: {}", message_id)?;
            writeln!(out, "References: {}", message_id)?;
        }
        in_reply_to_message_id = Some(cover_message_id);
        writeln!(out, "From: {} <{}>", committer_name, committer_email)?;
        writeln!(out, "Date: {}", date_822(committer.when()))?;
        writeln!(
            out,
            "Subject: [{}{}{:0>num_width$}/{}] {}\n",
            subject_patch,
            ensure_space(&subject_patch),
            0,
            commits.len(),
            subject,
            num_width=num_width,
        )?;
        if !body.is_empty() {
            writeln!(out, "{}", body)?;
        }
        writeln!(out, "{}", shortlog(&mut commits))?;
        writeln!(out, "{}", stats)?;
        writeln!(out, "base-commit: {}", base.id())?;
        writeln!(out, "{}", signature)?;
    }

    for (commit_num, commit) in commits.iter().enumerate() {
        let first_mail = commit_num == 0 && cover_entry.is_none();
        if to_stdout && !first_mail {
            writeln!(out)?;
        }

        let message = commit.message().unwrap();
        let (subject, body) = split_message(message);
        let commit_id = commit.id();
        let commit_author = commit.author();
        let commit_author_name = commit_author.name().unwrap();
        let commit_author_email = commit_author.email().unwrap();
        let summary_sanitized = sanitize_summary(&subject);
        let this_message_id = format!("<{}.{}>", commit_id, message_id_suffix);
        let parent = commit.parent(0)?;
        let diff = repo.diff_tree_to_tree(
            Some(&parent.tree().unwrap()),
            Some(&commit.tree().unwrap()),
            None,
        )?;
        let stats = diffstat(&diff)?;

        if !to_stdout {
            out = patch_file(&format!("{:04}-{}.patch", commit_num + 1, summary_sanitized))?;
        }
        writeln!(out, "From {} Mon Sep 17 00:00:00 2001", commit_id)?;
        writeln!(out, "Message-Id: {}", this_message_id)?;
        if let Some(ref message_id) = in_reply_to_message_id {
            writeln!(out, "In-Reply-To: {}", message_id)?;
            writeln!(out, "References: {}", message_id)?;
        }
        if first_mail {
            in_reply_to_message_id = Some(this_message_id);
        }
        if no_from {
            writeln!(out, "From: {} <{}>", commit_author_name, commit_author_email)?;
        } else {
            writeln!(out, "From: {} <{}>", committer_name, committer_email)?;
        }
        writeln!(out, "Date: {}", date_822(commit_author.when()))?;
        let prefix = if commits.len() == 1 && cover_entry.is_none() {
            if subject_patch.is_empty() {
                "".to_string()
            } else {
                format!("[{}] ", subject_patch)
            }
        } else {
            format!(
                "[{}{}{:0>num_width$}/{}] ",
                subject_patch,
                ensure_space(&subject_patch),
                commit_num + 1,
                commits.len(),
                num_width=num_width,
            )
        };
        writeln!(out, "Subject: {}{}\n", prefix, subject)?;

        if !no_from && (commit_author_name, commit_author_email) != (committer_name, committer_email) {
            writeln!(out, "From: {} <{}>\n", commit_author_name, commit_author_email)?;
        }
        if !body.is_empty() {
            write!(out, "{}{}", body, ensure_nl(&body))?;
        }
        writeln!(out, "---")?;
        writeln!(out, "{}", stats)?;
        write_diff(&mut out, &diffcolors, &diff, false)?;
        if first_mail {
            writeln!(out, "\nbase-commit: {}", base.id())?;
        }
        writeln!(out, "{}", signature)?;
    }

    Ok(())
}

fn log(out: &mut Output, repo: &Repository, m: &ArgMatches) -> Result<()> {
    let config = repo.config()?.snapshot()?;
    out.auto_pager(&config, "log", true)?;
    let diffcolors = DiffColors::new(out, &config)?;

    let shead_id = repo.refname_to_id(SHEAD_REF)?;
    let mut hidden_ids = std::collections::HashSet::new();
    let mut commit_stack = Vec::new();
    commit_stack.push(shead_id);
    while let Some(oid) = commit_stack.pop() {
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;
        for parent_id in commit.parent_ids() {
            if tree.get_id(parent_id).is_some() {
                hidden_ids.insert(parent_id);
            } else {
                commit_stack.push(parent_id);
            }
        }
    }

    let mut revwalk = repo.revwalk()?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL);
    revwalk.push(shead_id)?;
    for id in hidden_ids {
        revwalk.hide(id)?;
    }

    let show_diff = m.is_present("patch");

    let mut first = true;
    for oid in revwalk {
        if first {
            first = false;
        } else {
            writeln!(out)?;
        }
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let author = commit.author();

        writeln!(out, "{}", diffcolors.commit.paint(format!("commit {}", oid)))?;
        writeln!(out, "Author: {} <{}>", author.name().unwrap(), author.email().unwrap())?;
        writeln!(out, "Date:   {}\n", date_822(author.when()))?;
        for line in commit.message().unwrap().lines() {
            writeln!(out, "    {}", line)?;
        }

        if show_diff {
            let tree = commit.tree()?;
            let parent_ids: Vec<_> = commit.parent_ids().take_while(|parent_id| tree.get_id(*parent_id).is_none()).collect();

            writeln!(out)?;
            if parent_ids.len() > 1 {
                writeln!(out, "(Diffs of series merge commits not yet supported)")?;
            } else {
                let parent_tree = if parent_ids.is_empty() {
                    None
                } else {
                    Some(repo.find_commit(parent_ids[0])?.tree()?)
                };
                write_series_diff(out, repo, &diffcolors, parent_tree.as_ref(), Some(&tree))?;
            }
        }
    }

    Ok(())
}

fn rebase(repo: &Repository, m: &ArgMatches) -> Result<()> {
    match repo.state() {
        git2::RepositoryState::Clean => (),
        git2::RepositoryState::RebaseMerge
            if repo.path().join("rebase-merge").join("git-series").exists()
        => {
            return Err(concat!(
                "git series rebase already in progress.\n",
                "Use \"git rebase --continue\" or \"git rebase --abort\".",
            ).into());
        }
        s => return Err(format!("{:?} in progress; cannot rebase", s).into()),
    }

    let internals = Internals::read(repo)?;
    let series = internals.working.get("series")?
        .ok_or("Could not find entry \"series\" in working index")?;
    let base = internals.working.get("base")?
        .ok_or("Cannot rebase series; no base set.\nUse \"git series base\" to set base.")?;
    if series.id() == base.id() {
        return Err("No patches to rebase; series and base identical.".into());
    } else if !repo.graph_descendant_of(series.id(), base.id())? {
        return Err(format!(
            "Cannot rebase: current base {} not an ancestor of series {}",
            base.id(),
            series.id(),
        ).into());
    }

    // Check for unstaged or uncommitted changes before attempting to rebase.
    let series_commit = repo.find_commit(series.id())?;
    let series_tree = series_commit.tree()?;
    let mut unclean = String::new();
    if !diff_empty(&repo.diff_tree_to_index(Some(&series_tree), None, None)?) {
        writeln!(unclean, "Cannot rebase: you have unstaged changes.").unwrap();
    }
    if !diff_empty(&repo.diff_index_to_workdir(None, None)?) {
        if unclean.is_empty() {
            writeln!(unclean, "Cannot rebase: your index contains uncommitted changes.").unwrap();
        } else {
            writeln!(unclean, "Additionally, your index contains uncommitted changes.").unwrap();
        }
    }
    if !unclean.is_empty() {
        return Err(unclean.into());
    }

    let mut revwalk = repo.revwalk()?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE);
    revwalk.push(series.id())?;
    revwalk.hide(base.id())?;
    let commits: Vec<Commit> = revwalk.map(|c| {
        let id = c?;
        let mut commit = repo.find_commit(id)?;
        if commit.parent_ids().count() > 1 {
            return Err(format!(
                "Error: cannot rebase merge commit:\n{}",
                commit_obj_summarize(&mut commit)?,
            ).into());
        }
        Ok(commit)
    }).collect::<Result<_>>()?;

    let interactive = m.is_present("interactive");
    let onto = match m.value_of("onto") {
        None => None,
        Some(onto) => {
            let obj = repo.revparse_single(onto)?;
            let commit = obj.peel(ObjectType::Commit)?;
            Some(commit.id())
        }
    };

    let newbase = onto.unwrap_or(base.id());
    if newbase == base.id() && !interactive {
        println!("Nothing to do: base unchanged and not rebasing interactively");
        return Ok(());
    }

    let (base_short, _) = commit_summarize_components(&repo, base.id())?;
    let (newbase_short, _) = commit_summarize_components(&repo, newbase)?;
    let (series_short, _) = commit_summarize_components(&repo, series.id())?;

    let newbase_obj = repo.find_commit(newbase)?.into_object();

    let dir = tempdir::TempDir::new_in(repo.path(), "rebase-merge")?;
    let final_path = repo.path().join("rebase-merge");
    let mut create = std::fs::OpenOptions::new();
    create.write(true).create_new(true);

    create.open(dir.path().join("git-series"))?;
    create.open(dir.path().join("quiet"))?;
    create.open(dir.path().join("interactive"))?;

    let mut head_name_file = create.open(dir.path().join("head-name"))?;
    writeln!(head_name_file, "detached HEAD")?;

    let mut onto_file = create.open(dir.path().join("onto"))?;
    writeln!(onto_file, "{}", newbase)?;

    let mut orig_head_file = create.open(dir.path().join("orig-head"))?;
    writeln!(orig_head_file, "{}", series.id())?;

    let git_rebase_todo_filename = dir.path().join("git-rebase-todo");
    let mut git_rebase_todo = create.open(&git_rebase_todo_filename)?;
    for mut commit in commits {
        writeln!(git_rebase_todo, "pick {}", commit_obj_summarize(&mut commit)?)?;
    }
    if let Some(onto) = onto {
        writeln!(git_rebase_todo, "exec git series base {}", onto)?;
    }
    writeln!(git_rebase_todo, "\n# Rebase {}..{} onto {}", base_short, series_short, newbase_short)?;
    write!(git_rebase_todo, "{}", REBASE_COMMENT)?;
    drop(git_rebase_todo);

    // Interactive editor
    if interactive {
        let config = repo.config()?;
        run_editor(&config, &git_rebase_todo_filename)?;
        let mut file = File::open(&git_rebase_todo_filename)?;
        let mut todo = String::new();
        file.read_to_string(&mut todo)?;
        let todo = git2::message_prettify(todo, git2::DEFAULT_COMMENT_CHAR)?;
        if todo.is_empty() {
            return Err("Nothing to do".into());
        }
    }

    // Avoid races by not calling .into_path until after the rename succeeds.
    std::fs::rename(dir.path(), final_path)?;
    dir.into_path();

    checkout_tree(repo, &newbase_obj)?;
    repo.reference(
        "HEAD",
        newbase,
        true,
        &format!("rebase -i (start): checkout {}", newbase),
    )?;

    let status = Command::new("git").arg("rebase").arg("--continue").status()?;
    if !status.success() {
        return Err(format!("git rebase --continue exited with status {}", status).into());
    }

    Ok(())
}

fn req(out: &mut Output, repo: &Repository, m: &ArgMatches) -> Result<()> {
    let config = repo.config()?.snapshot()?;
    let shead = repo.find_reference(SHEAD_REF)?;
    let shead_commit = shead.resolve()?.peel_to_commit()?;
    let stree = shead_commit.tree()?;

    let series = stree.get_name("series")
        .ok_or("Internal error: series did not contain \"series\"")?;
    let series_id = series.id();
    let mut series_commit = repo.find_commit(series_id)?;
    let base = stree.get_name("base")
        .ok_or("Cannot request pull; no base set.\nUse \"git series base\" to set base.")?;
    let mut base_commit = repo.find_commit(base.id())?;

    let (cover_content, subject, cover_body) = if let Some(entry) = stree.get_name("cover") {
        let cover_blob = repo.find_blob(entry.id())?;
        let content = std::str::from_utf8(cover_blob.content())?.to_string();
        let (subject, body) = split_message(&content);
        (Some(content.to_string()), subject.to_string(), Some(body.to_string()))
    } else {
        (None, shead_series_name(&shead)?, None)
    };

    let url = m.value_of("url").unwrap();
    let tag = m.value_of("tag").unwrap();
    let full_tag = format!("refs/tags/{}", tag);
    let full_tag_peeled = format!("{}^{{}}", full_tag);
    let full_head = format!("refs/heads/{}", tag);
    let mut remote = repo.remote_anonymous(url)?;
    remote.connect(git2::Direction::Fetch)
        .map_err(|e| format!("Could not connect to remote repository {}\n{}", url, e))?;
    let remote_heads = remote.list()?;

    /* Find the requested name as either a tag or head */
    let mut opt_remote_tag = None;
    let mut opt_remote_tag_peeled = None;
    let mut opt_remote_head = None;
    for h in remote_heads {
        if h.name() == full_tag {
            opt_remote_tag = Some(h.oid());
        } else if h.name() == full_tag_peeled {
            opt_remote_tag_peeled = Some(h.oid());
        } else if h.name() == full_head {
            opt_remote_head = Some(h.oid());
        }
    }
    let (msg, extra_body, remote_pull_name) = match (opt_remote_tag, opt_remote_tag_peeled, opt_remote_head) {
        (Some(remote_tag), Some(remote_tag_peeled), _) => {
            if remote_tag_peeled != series_id {
                return Err(format!(
                    "Remote tag {} does not refer to series {}",
                    tag, series_id,
                ).into());
            }
            let local_tag = repo.find_tag(remote_tag)
                .map_err(|e| format!(
                    "Could not find remote tag {} ({}) in local repository: {}",
                    tag, remote_tag, e,
                ))?;
            let mut local_tag_msg = local_tag.message().unwrap().to_string();
            if let Some(sig_index) = local_tag_msg.find("-----BEGIN PGP ") {
                local_tag_msg.truncate(sig_index);
            }
            let extra_body = match cover_content {
                Some(ref content) if !local_tag_msg.contains(content) => cover_body,
                _ => None,
            };
            (Some(local_tag_msg), extra_body, full_tag)
        }
        (Some(remote_tag), None, _) => {
            if remote_tag != series_id {
                return Err(format!(
                    "Remote unannotated tag {} does not refer to series {}",
                    tag, series_id,
                ).into());
            }
            (cover_content, None, full_tag)
        }
        (_, _, Some(remote_head)) => {
            if remote_head != series_id {
                return Err(format!(
                    "Remote branch {} does not refer to series {}",
                    tag, series_id,
                ).into());
            }
            (cover_content, None, full_head)
        }
        _ => {
            return Err(format!("Remote does not have either a tag or branch named {}", tag).into())
        }
    };

    let commit_subject_date = |commit: &mut Commit| -> String {
        let date = date_822(commit.author().when());
        let summary = commit.summary().unwrap();
        format!("  {} ({})", summary, date)
    };

    let mut revwalk = repo.revwalk()?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE);
    revwalk.push(series_id)?;
    revwalk.hide(base.id())?;
    let mut commits: Vec<Commit> = revwalk
        .map(|c| Ok(repo.find_commit(c?)?))
        .collect::<Result<_>>()?;
    if commits.is_empty() {
        return Err("No patches to request pull of; series and base identical.".into());
    }

    let author = get_signature(&config, "AUTHOR")?;
    let author_email = author.email().unwrap();
    let message_id = format!(
        "<pull.{}.{}.git-series.{}>",
        shead_commit.id(),
        author.when().seconds(),
        author_email
    );

    let diff = repo.diff_tree_to_tree(
        Some(&base_commit.tree().unwrap()),
        Some(&series_commit.tree().unwrap()),
        None,
    )?;
    let stats = diffstat(&diff)?;

    out.auto_pager(&config, "request-pull", true)?;
    let diffcolors = DiffColors::new(out, &config)?;

    writeln!(out, "From {} Mon Sep 17 00:00:00 2001", shead_commit.id())?;
    writeln!(out, "Message-Id: {}", message_id)?;
    writeln!(out, "From: {} <{}>", author.name().unwrap(), author_email)?;
    writeln!(out, "Date: {}", date_822(author.when()))?;
    writeln!(out, "Subject: [GIT PULL] {}\n", subject)?;
    if let Some(extra_body) = extra_body {
        writeln!(out, "{}", extra_body)?;
    }
    writeln!(out, "The following changes since commit {}:\n", base.id())?;
    writeln!(out, "{}\n", commit_subject_date(&mut base_commit))?;
    writeln!(out, "are available in the git repository at:\n")?;
    writeln!(out, "  {} {}\n", url, remote_pull_name)?;
    writeln!(out, "for you to fetch changes up to {}:\n", series.id())?;
    writeln!(out, "{}\n", commit_subject_date(&mut series_commit))?;
    writeln!(out, "----------------------------------------------------------------")?;
    if let Some(msg) = msg {
        writeln!(out, "{}", msg)?;
        writeln!(out, "----------------------------------------------------------------")?;
    }
    writeln!(out, "{}", shortlog(&mut commits))?;
    writeln!(out, "{}", stats)?;
    if m.is_present("patch") {
        write_diff(out, &diffcolors, &diff, false)?;
    }
    writeln!(out, "{}", mail_signature())?;

    Ok(())
}

fn main() {
    let m = App::new("git-series")
            .bin_name("git series")
            .about("Track patch series in git")
            .author("Josh Triplett <josh@joshtriplett.org>")
            .version(clap::crate_version!())
            .global_setting(AppSettings::ColoredHelp)
            .global_setting(AppSettings::UnifiedHelpMessage)
            .global_setting(AppSettings::VersionlessSubcommands)
            .subcommands(vec![
                SubCommand::with_name("add")
                    .about("Add changes to the index for the next series commit")
                    .arg_from_usage("<change>... 'Changes to add (\"series\", \"base\", \"cover\")'"),
                SubCommand::with_name("base")
                    .about("Get or set the base commit for the patch series")
                    .arg(Arg::with_name("base").help("Base commit").conflicts_with("delete"))
                    .arg_from_usage("-d, --delete 'Clear patch series base'"),
                SubCommand::with_name("checkout")
                    .about("Resume work on a patch series; check out the current version")
                    .arg_from_usage("<name> 'Patch series to check out'"),
                SubCommand::with_name("commit")
                    .about("Record changes to the patch series")
                    .arg_from_usage("-a, --all 'Commit all changes'")
                    .arg_from_usage("-m [msg] 'Commit message'")
                    .arg_from_usage("-v, --verbose 'Show diff when preparing commit message'"),
                SubCommand::with_name("cover")
                    .about("Create or edit the cover letter for the patch series")
                    .arg_from_usage("-d, --delete 'Delete cover letter'"),
                SubCommand::with_name("cp")
                    .about("Copy a patch series")
                    .arg(Arg::with_name("source_dest").required(true).min_values(1).max_values(2).help("source (default: current series) and destination (required)")),
                SubCommand::with_name("delete")
                    .about("Delete a patch series")
                    .arg_from_usage("<name> 'Patch series to delete'"),
                SubCommand::with_name("detach")
                    .about("Stop working on any patch series"),
                SubCommand::with_name("diff")
                    .about("Show changes in the patch series"),
                SubCommand::with_name("format")
                    .about("Prepare patch series for email")
                    .arg_from_usage("--in-reply-to [Message-Id] 'Make the first mail a reply to the specified Message-Id'")
                    .arg_from_usage("--no-from 'Don't include in-body \"From:\" headers when formatting patches authored by others'")
                    .arg_from_usage("-v, --reroll-count=[N] 'Mark the patch series as PATCH vN'")
                    .arg(Arg::from_usage("--rfc 'Use [RFC PATCH] instead of the standard [PATCH] prefix'").conflicts_with("subject-prefix"))
                    .arg_from_usage("--stdout 'Write patches to stdout rather than files'")
                    .arg_from_usage("--subject-prefix [prefix] 'Use [prefix] instead of the standard [PATCH] prefix'"),
                SubCommand::with_name("log")
                    .about("Show the history of the patch series")
                    .arg_from_usage("-p, --patch 'Include a patch for each change committed to the series'"),
                SubCommand::with_name("mv")
                    .about("Move (rename) a patch series")
                    .visible_alias("rename")
                    .arg(Arg::with_name("source_dest").required(true).min_values(1).max_values(2).help("source (default: current series) and destination (required)")),
                SubCommand::with_name("rebase")
                    .about("Rebase the patch series")
                    .arg_from_usage("[onto] 'Commit to rebase onto'")
                    .arg_from_usage("-i, --interactive 'Interactively edit the list of commits'")
                    .group(ArgGroup::with_name("action").args(&["onto", "interactive"]).multiple(true).required(true)),
                SubCommand::with_name("req")
                    .about("Generate a mail requesting a pull of the patch series")
                    .visible_aliases(&["pull-request", "request-pull"])
                    .arg_from_usage("-p, --patch 'Include patch in the mail'")
                    .arg_from_usage("<url> 'Repository URL to request pull of'")
                    .arg_from_usage("<tag> 'Tag or branch name to request pull of'"),
                SubCommand::with_name("status")
                    .about("Show the status of the patch series"),
                SubCommand::with_name("start")
                    .about("Start a new patch series")
                    .arg_from_usage("<name> 'Patch series name'"),
                SubCommand::with_name("unadd")
                    .about("Undo \"git series add\", removing changes from the next series commit")
                    .arg_from_usage("<change>... 'Changes to remove (\"series\", \"base\", \"cover\")'"),
            ]).get_matches();

    let mut out = Output::new();

    let err = || -> Result<()> {
        let repo = Repository::discover(".")?;
        match m.subcommand() {
            ("", _) => series(&mut out, &repo),
            ("add", Some(ref sm)) => add(&repo, &sm),
            ("base", Some(ref sm)) => base(&repo, &sm),
            ("checkout", Some(ref sm)) => checkout(&repo, &sm),
            ("commit", Some(ref sm)) => commit_status(&mut out, &repo, &sm, false),
            ("cover", Some(ref sm)) => cover(&repo, &sm),
            ("cp", Some(ref sm)) => cp_mv(&repo, &sm, false),
            ("delete", Some(ref sm)) => delete(&repo, &sm),
            ("detach", _) => detach(&repo),
            ("diff", _) => do_diff(&mut out, &repo),
            ("format", Some(ref sm)) => format(&mut out, &repo, &sm),
            ("log", Some(ref sm)) => log(&mut out, &repo, &sm),
            ("mv", Some(ref sm)) => cp_mv(&repo, &sm, true),
            ("rebase", Some(ref sm)) => rebase(&repo, &sm),
            ("req", Some(ref sm)) => req(&mut out, &repo, &sm),
            ("start", Some(ref sm)) => start(&repo, &sm),
            ("status", Some(ref sm)) => commit_status(&mut out, &repo, &sm, true),
            ("unadd", Some(ref sm)) => unadd(&repo, &sm),
            _ => unreachable!(),
        }
    }();

    if let Err(e) = err {
        let msg = e.to_string();
        out.write_err(&format!("{}{}", msg, ensure_nl(&msg)));
        drop(out);
        std::process::exit(1);
    }
}
