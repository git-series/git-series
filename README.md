git series tracks changes to a patch series over time.  git series also tracks
a cover letter for the patch series, formats the series for email, and prepares
pull requests.

[Manpage for git-series](http://man7.org/linux/man-pages/man1/git-series.1.html)

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

Overview of commands
====================

- Use `git series start seriesname` to start a patch series seriesname.

- Use `git series base somecommit` to set the base commit for the series.
  (This is the upstream commit you based the series on, not the first patch in
  the series.)

- Use normal git commands to commit changes.

- Use `git series status` to check what has changed.

- Use `git series cover` to add or edit a cover letter.

- Use `git series rebase -i` to help rework or reorganize the patch series.

- Use `git series add` and `git series commit` (or `git series commit -a`) to
  commit changes to the patch series.  You can do this whenever you've changed
  the base or cover letter, or whenever you've changed HEAD to a new commit.
  Make a series commit whenever you've made a semantic change to the patch
  series that you want to record, such as rebasing on a new upstream version,
  reorganizing patches, or incorporating feedback.

- Use `git series format` to prepare the patch series to send via email, or
  use `git series req` to prepare a "please pull" mail (after pushing the
  changes to a repository as a branch or tag).

Workflow example
================

Suppose you want to write a patch series implementing a new feature for a
project.  You already have a local `git clone` of the repository.  You could
start a branch for this patch series, but it may take multiple iterations
before upstream accepts it, and you may need to use rebase or amend to fix
commits; a branch can't track that.  With git-series, you'll develop the patch
series as you normally would, including rebases, and periodically `git series
commit` the state of the patch series, complete with a commit message
explaining what you've changed.  Even if you rebase the patch series, or make
some other change that doesn't fast-forward, git-series will track those
changes with a branch that *does* fast-forward, so you can easily share and
review the history of your patch series.

Developing or importing the first version
-----------------------------------------

To start the patch series, first run `git series start featurename`.
`featurename` here specifies the name for the series, just as you'd specify the
name of a branch.

A patch series needs some base to build on, identifying the upstream commit you
want to develop from.  This will become the parent of the first patch in your
series.  If you want to base your patch series on the current version, run `git
series base HEAD`.  If you want to base this patch series on some other commit,
such as a released version, first check out that commit with `git checkout
thecommit`, then run `git series base HEAD`.

You can then develop the patch series as usual, committing patches with git.

If you've already started on the patch series and made some commits, you can
still adopt the current version of the patch series into git-series.  Find the
parent commit of the first patch in your series, and run `git series base
thatcommit`.

As with `git`, you can run `git series status` at any time to see the current
state of the series, including changes you might want to commit, and the next
step to run.  After the above steps, `git series status` should show `base` and
`series` modified; running `git series base` set the `base` in the "working"
version, and `series` in the working version always refers to HEAD (the current
git commit you have checked out).

Now that you've written an initial version of the patch series (or you already
wrote it before you started using git-series), you can commit that version to
git-series.  Run `git series commit -a` to commit the series.  This will run
your editor so you can provide a series commit message (e.g. "Initial version
of feature" or "Import feature into git-series").

If your patch series include multiple patches, you may want to add a cover
letter.  Run `git series cover` to edit the cover letter, then `git series
commit -a -m 'Add cover letter'` to commit that change to the series.

Now that you have the first version of the patch series, you can format it as a
series of emails with `git series format`.

Developing v2
-------------

You send the patch series by email, and you get feedback from the maintainers:
the concept looks good, but you need to split one of the patches into two, and
add benchmark results in another commit's commit message.

Run `git series rebase -i`, and split the commit (mark it for 'e'dit, `git
reset -N HEAD^`, repeatedly `git add -p` and `git commit`, then `git rebase
--continue`).  Then, commit that change to the series: `git series commit -a -m
'Split out X change into a separate patch'`

Then, run `git series rebase -i` again to add the benchmark results (mark the
commit for 'r'eword), and commit that change: `git series commit -a -m 'Add
benchmark results'`.

You may want to document the changes in the cover letter: run `git series
cover`, document the changes, and `git series commit -a -m 'Update cover letter
for v2'`.  (Alternatively, you can incrementally add to the cover letter along
with each change to the series.)

Now that you have v2 of the patch series, you can format it as a new series of
emails with `git series format -v 2`.
