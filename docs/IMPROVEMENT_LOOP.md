# Autonomous improvement loop

North star: **people enjoy using Slipstream every day** — a large local coding model that
answers fast, answers correctly, and takes no effort to set up. Every change must move a
number on `bench/SCOREBOARD.md` without making another one worse.

## One cycle (run on every resume ping when nothing heavy is running)

1. **Read** `bench/SCOREBOARD.md`, `docs/BACKLOG.md`, the end of `bench/RESULTS.md`,
   `CLAUDE.md`, and the work_state list `slipstream-fetch-wall`.
2. **Pick** the open backlog item with the highest score =
   user impact (1–5) × confidence (1–5) ÷ effort (1–5). Ties: prefer lossless, then the
   metric furthest from its target. Mark it `doing`.
3. **Assume** — write the hypothesis as a measurable target before touching code
   ("TTFT 35k prompt 150 s → < 60 s, output identical").
4. **Research** when the idea is new: Colibrì upstream, llama.cpp upstream, alphaxiv.
   Prefer adapting merged, measured code over inventing (Colibrì-first rule).
5. **Build** behind an env flag or setting, default off. Wrap heavy work in
   `scripts/heavy.sh`.
6. **Measure** with the `bench-ab` skill in the regime the app actually runs
   (llama-server, app flags, internal SSD): interleaved, ≥ 2 reps, swap growth 0 MB,
   lossless ⇒ identical output hash, lossy ⇒ KL / top-1 / perplexity gate.
7. **Challenge** — before keeping anything, spawn one fresh reviewer agent with only the
   diff, the raw numbers and the method; its job is to find why the result is wrong
   (noise, wrong regime, cherry-picked prompt, hidden output change, regressions elsewhere).
   Fix or re-measure what it finds.
8. **Decide** — keep only consistent wins (make lossless wins default-on; lossy wins stay
   opt-in until the user decides), revert everything else. Record winners *and* losers in
   `bench/RESULTS.md`, update `bench/SCOREBOARD.md`, run `scripts/engine_sync.sh --verify`,
   commit locally on `feat/fetch-wall`.
   If the win changes what a user notices, update the website (`docs/index.html`), README and
   BENCHMARKS.md in plain language in the same change (rule in CLAUDE.md "Public docs in plain
   language").
9. **Learn** — add new ideas discovered along the way to the backlog with scores; put any
   lesson that should change future behaviour into `CLAUDE.md` (Lessons) or memory.
10. **Report** in two or three German sentences: what was tried, the number, the verdict.

## Recurring sweeps

- **Weekly:** research scan — new Colibrì commits/PRs, llama.cpp upstream distance,
  dependency updates, new arXiv work on MoE offloading — into the backlog.
- **Every UI change, and monthly:** click through the first-run journey as a new user with
  the Tauri mock (`app/ui-review/JOURNEY.md`); every extra click or unclear word is a
  backlog item.

## Guardrails (never broken by the loop)

- Push, merge to `main`, tag and release are allowed autonomously (user, 2026-11-26) for
  verified milestones only: all tests green, parity held, wins recorded with numbers.
  Every release is complete: dmg + zip + node tarball, verified on the mounted DMG,
  checksums in docs/RELEASE_<v>.md. Never a release without the .dmg.
- No lossy behaviour on by default without an explicit user decision.
- Models only from the internal SSD; one model process at a time; never bypass RAM admission.
- No deleting user data; destructive operations need the user.
- If everything open is blocked on a user decision: do nothing new, report status.
