git-series internals
====================

Requirements
------------

The format git-series uses to store each patch series ensures that standard git
tools can always handle a git-series repository.  In particular:

- All commits and objects in the history and metadata of every series must
  remain reachable via git's normal object reachability algorithms, so that git
  will never discard the history or metadata of a series.
- Transferring a git-series repository via git's standard protocols must
  transfer all series including history and metadata, without any extensions to
  the git protocols.

Refs
----

git-series stores the series ref for a patch series named `NAME` in
`refs/heads/git-series/NAME`.  This will appear to git as a branch named
`git-series/NAME`.  From that ref, git can reach all the information git-series
tracks about a patch series, so sending or receiving that ref brings along all
the information git-series needs.

git-series maintains a symbolic ref `refs/SHEAD` pointing to the current
series.  If a repository does not have a current series, SHEAD will not exist.

git-series commits
------------------

git-series stores each version of a patch series as one commit object.  The
`git-series/NAME` ref refers to commit corresponding to the current version of
the patch series NAME.  The tree object within each git-series commit acts like
a key-value store, with tree entry names as keys; the tree entry `series`
references the last commit of the patch series itself.

In this documentation, a "git-series commit" refers to a commit corresponding
to a version of an entire patch series, as distinguished from a commit
corresponding to one patch within a patch series.

The first parent of each git-series commit always points to the previous
version of the patch series, if any.  The remaining parents of each git-series
commit correspond to commits referenced as gitlinks (tree entries with mode
160000) within the commit's tree.  This ensures that git can reach all of those
commits.  (Note that git's traversal algorithm does not follow gitlink commits
within tree objects, so without these additional parent links, git would
consider these gitlink commits unreachable and discard them.)

The second and subsequent parents of each git-series commit do not appear in
any particular order; do not assume that the `series` object or any other
gitlink appears at any particular position within the parents list.  These
parents exist only to make commits reachable and transferable by git.  Always
look up commits via named tree entries within the git-series commit's tree
object.

In the root git-series commit, all the parent commits correspond to gitlinks
within the tree.  This will not occur for any non-root commit of a git-series.
Algorithms trying to walk from a git-series commit to its root should detect
the root git-series commit by checking if the first parent appears in the
git-series commit's tree.  (This does not require a recursive tree walk; the
first parent of the git-series root will always appear in the top-level tree
object.)

git-series tree entries
-----------------------

The tree within a git-series commit can contain the following entries:

- `series`: Every git-series tree must contain this entry, as a gitlink with
  mode 160000.  This identifies the last commit in the patch series.
- `base`: If this exists, it must refer to a gitlink with mode 160000.  This
  identifies the base commit for the patch series.  The patch series consists
  of the commits reachable from `series` and not reachable from `base`:
  `base`..`series`.  Many git-series commands require `base`, but a patch
  series does not have to have a `base`.
- `cover`: If this exists, it must refer to a blob with mode 100644.  This
  provides a cover letter for the patch series.  This blob should contain UTF-8
  text.

git-series staged changes and "working directory"
-------------------------------------------------

Like git, git-series allows staging part of all of the changes to the patch
series for a commit, or committing all the changes directly via `git series
commit -a`.  However, git-series does not maintain a "working directory"
directly.  Instead, git-series tracks the staged and unstaged changes to a
patch series named NAME via commits referenced by
`refs/git-series-internals/staged/NAME` and
`refs/git-series-internals/working/NAME`.  The tree of each of those commits
may contain any of the standard git-series tree entries.  (If the series has
nothing staged, the "staged" ref will not exist.)  These commits will also have
all of the corresponding gitlink entries as parents, to keep them reachable by
git.

The `working` commit for a patch series tracks the current state of the patch
series.  For example, setting a base with `git series base` or a cover letter
with `git series cover` will store the new base or cover letter as `base` or
`cover` in the tree of the commit referenced from the working ref.  git-series
commands will keep the `series` entry of the working tree referring to the
current HEAD.

The `staged` commit for a patch series, if present, tracks the staged changes
to the patch series.  `git series add` adds changes from `working` to `staged`,
and `git series unadd` removes changes from `staged`.

If a series does not have a series ref `refs/git-series/NAME`, but has a staged
or working ref, the series still exists, with no series commits.  This can
happen by running `git series start NAME`, making some changes without
committing, and then running `git series detach`.  git-series treats that as an
existing series, and allows checking it out.  This preserves work in progress
on an un-started series.
