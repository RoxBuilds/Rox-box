# Roadmap

Where Rox-box is going, why, and what to work on next. This file is the plan
of record; the open [issues](https://github.com/RoxBuilds/Rox-box/issues)
labelled `roadmap` are its tasks, one issue per task, each written so that
someone new can start on it. [ARCHITECTURE.md](ARCHITECTURE.md) is the map of
the code and [CONTRIBUTING.md](CONTRIBUTING.md) the rules.

## The goal

**Any Direct3D 12.X title you own, playable on an ordinary Windows 11 gaming
PC, from one folder with nothing to install, with every step towards that
measurable by anyone, without sharing a byte of a title.**

Three audiences, three promises:

- **Players**: unzip, double-click, pick your folder. The layer tells you
  honestly what your PC can and cannot do before you spend an evening on it,
  and when something goes wrong it hands you a file that is safe to post.
- **Developers**: the most approachable codebase of its kind. The whole frame
  path runs in CI and on your PC without any title files; every behaviour
  change sits behind a switch; every claim about speed comes with a number
  anyone can reproduce.
- **The project**: a clean-room, GPL-3.0-or-later, non-commercial,
  title-agnostic platform layer that can stay public indefinitely, because
  nothing proprietary ever enters it.

"Playable" is meant literally: a steady 30 frames per second on a mid-range
card, without device losses, with the picture the title intends.

## Where we are

Honest numbers, in relative terms:

- It starts some titles and reaches gameplay. Frame rates are in the low
  double digits on a high-end 64-lane desktop card with plenty of memory, and
  in the low single digits on 8 GB / 32-lane cards in PCs with little RAM.
- The published build has been run end to end on a title on a 32-lane card:
  it reaches the world, slowly and with a picture that is wrong in places. Its
  report says where the frame goes: the card busy most of the time, the
  submitting side waiting for it for one main cause, and a PC out of physical
  memory ([docs/findings.md](docs/findings.md)). A report from a 64-lane card
  is still wanted.
- The path every player takes is tested without a title: a self-test of five
  stages and a demo title of our own source, run by `victory run` in the
  tests and in CI on the software adapter.
- A run can be shared: every run and every self-test writes a report of
  numbers and our own words, and two reports compare with one command.
- Setup needs no terminal and no file editing; shaders and pipelines are
  prepared by the launcher.
- Changes made from that first report (small-card texture policy, the wait
  for unwatched memory, a title's own command line and exits) are tested on
  synthetic data and on the demo title; **runs of a title have not confirmed
  them yet**.

## Strategy

The order matters. Each step makes the next one cheaper.

### 1. Measure in the open

The rule that keeps this project legal (nothing from a title, ever) also made
it blind. Fixing that comes first, because every other piece of work is
steered by it:

- a **run report**: numbers and our own words only, written by every run,
  safe to attach by construction ([docs/run-report.md](docs/run-report.md));
- a **self-test**: a synthetic title made of our own code that drives the real
  frame path and benchmarks the layer's hot paths, so a graphics card, a
  driver or a pull request can be judged with no title at all
  ([docs/selftest.md](docs/selftest.md));
- issue forms built around those two outputs.

### 2. Close the loop without a title

A change should be provable on a contributor's PC and in CI:

- an **open demo title**: a real PE32+ image built from our own source that
  goes through the loader, the runtime and the graphics layer with synthetic
  console shaders. It turns the self-test's in-process scene into a full run
  and is the benchmark every performance claim is made against. It exists
  (`tests/fixtures/demo-title`, [docs/demo-title.md](docs/demo-title.md)), and
  `cargo test --workspace` runs it with `victory run` on the software
  adapter;
- **benchmark gates** in CI: the self-test's rates and the demo title's frame
  times, compared with the previous commit;
- **frame captures of the demo title** for the replay tool
  ([docs/replay.md](docs/replay.md)), so GPU experiments take seconds.

### 3. Remove the unified-memory tax

A console's CPU and GPU share memory; a PC's do not. Nearly all of a frame's
cost here is the price of pretending they do
([docs/findings.md](docs/findings.md)): tracking what the CPU wrote, tracking
what shaders wrote, copying both ways, and keeping it ordered. Polishing those
four costs has diminishing returns. The structural moves are:

- **Know writes instead of detecting them.** The shader translator sees every
  store a shader can make. Emitting an exact written-range description per
  pipeline lets the graphics layer skip compare and hash passes over memory a
  shader provably cannot touch.
- **Share instead of copy.** Open title memory as card memory wherever the
  host allows it (`inplace.rs` does so for part of it today) and keep buffers
  that only the card reads resident on the card.
- **Pipeline the submission thread.** Preparing a submission is serial today;
  split it into stages that overlap with the card's execution of the previous
  one.
- **Budget memory as a first-class resource.** On PCs with little RAM the
  shared-memory budget, not the card, is the wall. `host/budget.rs`,
  `host/commit.rs` and `host/residency.rs` each decide alone today: make
  them one governor for commit, shared and card memory, with degradation
  that costs speed, never correctness.

### 4. Make translated shaders cheap and complete

- an optimisation pass over the translator's own SSA (`ir.rs` is one to one
  with the native code; today's clean-up runs only after lowering, in
  `emit/bitcode/opt.rs`);
- exact, fast forms for 64-lane programs on 32-lane cards;
- the programs that do not translate yet: merged geometry stages and
  descriptors read as data;
- mesh-shader fallbacks for cards without them.

### 5. Zero-friction play

- setup with no terminal and no file editing;
- shader translation and pipeline warm-up as automatic, resumable background
  steps with progress, not commands to learn;
- presets chosen from the self-test's result instead of by hand.

## Milestones

Each has an exit test that someone else can run.

| | Milestone | Exit test |
|---|---|---|
| **0.2** | **Measure in the open.** Run report, self-test, issue forms, first-run setup | A stranger files a useful issue with a report and a self-test result, breaking no rule. CI runs the frame path on the software adapter |
| **0.3** | **Verified baseline.** A full run of the public build re-verified on real hardware; open demo title through the loader; benchmark gates in CI; automatic shader and pipeline preparation | `victory run` of the demo title passes in CI. A report from a real run exists for at least one 64-lane and one 32-lane card |
| **0.5** | **Usable on 8 GB cards.** Memory governor, written-range descriptions, staged submission | On an 8 GB / 32-lane card in a 16 GB PC: thirty minutes without a device loss or an out-of-memory stop, reports showing a steady double-digit frame rate at reduced resolution |
| **0.8** | **Playable.** Shared memory wherever the host allows it, shader optimisation, complete translation of the common program shapes | Reports from mid-range cards at 30 frames per second with no draws left out in steady state |
| **1.0** | **Breadth.** The platform surface most titles need (`victory census` coverage), stable saves, audio without drift, controller features | The demo title and community reports cover each API area; no known device-loss cause open |

Version numbers are intentions, not dates. This is volunteer, non-commercial
work.

## How to help without owning anything

Most of the roadmap needs no title files at all:

- **Run the self-test** on your PC and post the result with the
  hardware-report issue form. Every card and driver family we have not seen
  is useful.
- **Pick a `good first issue`**: launcher improvements, documentation, tests,
  the benchmark set.
- **Work on the translator** with synthetic programs: its tests build their
  own input.
- **Work on the demo title**: it is ordinary Rust and shader code of our own.
- **Review**: measurement claims, unsafe code, the privacy guard of the run
  report.

With a title of your own you can do more, under the same rules: fix what your
run report shows, and describe it in platform terms. Never name the title.

## Not goals

- Online services. Titles see an offline console.
- Anything that obtains, decrypts or redistributes title files, or works
  around copy protection.
- Per-title code. If one title needs different behaviour, the platform rule
  that explains it is the thing to implement; what is left goes into a file
  the user supplies.
- Shipping proprietary runtimes or SDK components to lift a host limit.
- Payment, sponsorship or advertising of any kind.
