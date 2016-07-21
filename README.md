git series tracks changes to a patch series over time.  git series also tracks
a cover letter for the patch series, formats the series for email, and prepares
pull requests.

About git-series
================

A patch series typically goes through multiple iterations before submission;
the path from idea to RFC to `[PATCHv12 1/8]` includes many invocations of
`git rebase -i`. However, while Git tracks and organizes commits quite well, it
doesn't actually track changes to a patch series at all, outside of the
ephemeral reflog.  This makes it a challenge to collaborate on a patch series,
distribution package, backport, or any other development process that includes
rebasing or non-fast-forward development.

Typically, tracking the evolution of a patch series over time involves moving
part of the version control outside of git.  You can move the patch series from
git into quilt or a distribution package, and then version the patch files with
git, losing the power of git's tools.  Or, you can keep the patch series in
git, and version it via multiple named branches; however, names like
feature-v2, feature-v3-typofix, and feature-v8-rebased-4.6-alice-fix sound like
filenames from corporate email, not modern version control.  And either way,
git doesn't track your cover letter at all.

git-series tracks both a patch series and its evolution within the same git
repository.  git-series works entirely with existing git features, allowing git
to push and pull a series to any git repository along with other branches and
tags.  git-series also tracks a cover letter for the patch series, formats the
series for email, and prepares pull requests.

Building and installing
=======================

git-series is written in Rust.  You'll need both Rust and Cargo installed to
build it.  If your OS distribution includes packages for Rust and Cargo, start
by installing those (for instance, on Debian, `apt install rustc cargo`).
Otherwise, you can [download the stable version of Rust and Cargo from the
rust-lang.org download page](https://www.rust-lang.org/downloads.html).

With Rust and Cargo installed, you can download and install the latest release
of git-series with:

```
cargo install --root ~/.local git-series
```

This will install git-series into `~/.local/bin/git-series`.  If you don't
already have `~/.local/bin` on your `$PATH`, you may want to add it there, or
change the `--root`.  You may also want to install the included manpage,
`git-series.1`, into `~/.local/share/man/man1/git-series.1`.

If you'd like to package git-series for your distribution, please contact me.

Getting started
===============

- Use `git series start seriesname` to start a patch series seriesname.

- Use `git series base somecommit` to set the base commit for the series.
  (This is the upstream commit you based the series on, not the first patch in
  the series.)

- Use normal git commands to commit changes.

- Use `git series status` to check what has changed.

- Use `git series cover` to add or edit a cover letter.

- Use `git series add` and `git series commit` (or `git series commit -a`) to
  commit changes to the patch series.

- Use `git series rebase -i` to help rework or reorganize the patch series.

- Use `git series format` to prepare the patch series to send via email, or
  use `git series req` to prepare a "please pull" mail (after pushing the
  changes to a repository as a branch or tag).
