# How your work reaches the branch

There are two ways this service can publish finished work. You choose per
repository, and you can change your mind later. Neither choice affects how work
is captured, verified, or scheduled — only what happens at the moment of
publication.

## The short version

| | **Direct push** | **Pull request** *(not yet implemented)* |
| --- | --- | --- |
| Works with | Any Git host | GitHub |
| Needs a token | No | Yes, one you create |
| Protected `main` | Push is refused; you are told why | Handled — that is the point |
| At the scheduled time | Your commit appears on the target | A branch is pushed and a PR is opened |
| Merging | Not applicable | **You merge, by hand, always** |

If you are publishing to your own repository and nothing blocks a push, use
**direct push**. It is simpler and there is no token to manage.

If your target branch requires a pull request — most shared repositories do —
use **pull request**.

## Direct push

The default. At the release time you chose, the service creates the commit and
pushes it to your target branch, using the same Git credentials you already use.
Nothing new is stored and nothing leaves your machine except the push itself.

If the host refuses the push because the branch is protected, the service says
so plainly, keeps your work queued, and tells you what to do about it. It does
not retry forever and it does not quietly drop the change.

This works with GitHub, GitLab, Bitbucket, Gitea, a bare repository on a server
you own — anything Git can push to.

### Two timings: straight to the target, or early to a development branch

Direct push has two modes, chosen when you enrol a repository and changeable
later.

```
reccursive repository add ~/code/my-project                      # scheduled (default)
reccursive repository add ~/code/my-project \
    --mode immediate --development-target development            # immediate
```

**Scheduled** (`--mode scheduled`) is the default: at the release time, the
commit is created and pushed to your target branch. One step.

**Immediate** (`--mode immediate`) publishes to a development branch instead, at
the same release time. Your target branch is not touched. The work is on the
remote where you and anyone else can see it, run CI against it, and review it —
but nothing has landed on `main`. Integrating it into the target is a separate,
later step.

Immediate mode needs `--development-target`, and it must be a different branch
from your target. The branch does not have to exist: the first publication
creates it from your target branch, and every later one stacks on top of it.

Integration happens on its own. Once work is on the development branch, the
service selects a second release time from the same policy and, when it arrives,
publishes the same work to your target branch. You do not schedule the second
half by hand. Until that lands, `queue status` and `feature status` say where
the work actually is — `available early on refs/heads/development` rather than
anything that could be read as finished.

This also makes dependencies between tasks more precise. A plan can say a task
waits for another to be `development_available` — on the development branch —
rather than `target_published`. The first is satisfied as soon as the
prerequisite is pushed to the development branch; the second only once the
target itself carries it. In scheduled mode, where there is no development
branch, publishing to the target satisfies both.

## Pull request — not yet implemented

**This strategy is being built and is not available yet.** It is described here
because the design is settled and the trade-off it asks you to make is one you
should be able to read before you depend on this product. Everything below says
what it *will* do. Until it ships, direct push is the only strategy, and
`--mode immediate` publishes to a development branch without opening anything.

At the release time you chose, the service pushes your work to a branch and
opens a pull request against your target. You get a notification from GitHub the
way you would for any PR, review it when you like, and merge it yourself.

**The service never merges.** Not when checks pass, not on a schedule, not
ever. Opening a pull request is automation; merging one is authority over what
lands on your main branch, and this product does not take that. If you want a
change merged, you merge it.

### What the token is for, and what it can do

Opening a pull request needs a GitHub token, because Git alone cannot create
one. You create it, you scope it, and you can revoke it at any time.

Be clear-eyed about what this changes. Without a token, the honest description
of this product is *it uses your Git credentials and nothing else*. With one,
it becomes *that, plus a GitHub token you chose to give it, used only to open
pull requests*. The token is stored with owner-only permissions alongside the
service's own local credentials, and is never written into logs, events, or the
diagnostic export. But it is a real secret on your machine, and a GitHub token
generally reaches further than push access does. Give it the narrowest scope
GitHub will let you.

If that trade is not one you want to make, direct push is a complete product and
always will be.

## Switching

Changing strategy affects work that has not published yet. The service will not
let you switch in a way that would duplicate a unit, rewrite something already
published, or send queued work to a branch you did not intend — it tells you
what would happen and asks you to resolve it first.

## What this does not do

- It does not merge pull requests.
- It does not approve or dismiss reviews.
- It does not change branch protection rules, or ask to.
- It does not talk to any host but GitHub, and only when you have chosen the
  pull-request strategy for that repository.
