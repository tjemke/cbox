# Working on cbox

## Commits

One logical change per commit, each of which builds and evaluates on its own.
Subjects are lowercase and imperative, prefixed with the area they touch
(`guest:`, `run:`), or with none when a change cuts across areas. The body says
why, wrapped at 72 columns. End each message with the co-author trailer your
harness asks for.

Inside a cbox, `/work/.git` is read-only unless the session was started with
`--git rw`, so you can't commit. When asked to commit there, write a commit plan
instead, and the maintainer runs it on the host:

- `commit-plan.sh` at the repo root, untracked. If an older plan is there and
  its commits aren't in `git log` yet, ask before replacing it.
- It pins the base commit, and stops if HEAD has moved or the index has staged
  changes.
- Each commit's patch and message go inline, in quoted heredocs.
- It replays the whole series on a scratch index (`GIT_INDEX_FILE`) and commits
  nothing unless the result matches the working tree. Then it runs
  `git apply --cached` and `git commit -F` for each commit. It writes only the
  index and HEAD, never the working tree.
- Test it before handing it over: run it in a `git clone /work` with the
  working-tree files copied in, check that it reproduces your commits, and
  check that it refuses a tree edited since.

The maintainer runs it on their host, so keep it plain enough to read.
