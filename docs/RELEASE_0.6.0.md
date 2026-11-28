# Slipstream 0.6.0 — use the model on your other Mac (experimental)

Your MacBook's coding editor can now use the model running on a bigger Mac in the same network —
a Mac Studio or Mac mini at home or in the office. The editor keeps its address
(`http://127.0.0.1:8080/v1`); the answers come from the other Mac. Only your own Macs, paired with
a 6-digit code, can connect, and nothing goes to the internet.

> **Experimental.** Tested on one Mac with two instances. Not yet tested between two real Macs
> (network permission prompt, real Wi-Fi, sleep/wake). If you have two Macs, please tell us how it
> works for you: <https://github.com/Schero94/slipstream/issues>.

The engine is unchanged from 0.5.3; the models, speeds and answers on a single Mac stay the same.

## What's new

- **"Your Macs" tab** (was "Cluster"). On the big Mac: **Share this Mac** → a 6-digit code appears
  (1 click). On the MacBook: **Find my Macs** → type the code (2 clicks; 3 if a local model is
  running or several Macs are found). After that, nothing to click: the MacBook reconnects every
  time the app starts.
- **Your editor does not change.** The Connect-editor setting now always includes this install's
  local key. The model on this Mac ignores it, the connection to the other Mac requires it — so the
  same setting works locally and with the other Mac. If the other Mac's model has a different
  context size, the app offers a one-click update of the editor settings.
- **Private by design.** Pairing uses a password protocol (SPAKE2) on the 6-digit code; a wrong code
  three times closes the window, the code expires after 5 minutes. Each Mac remembers the other's
  identity and refuses everyone else before reading a request. Traffic between the Macs is encrypted
  (QUIC with TLS 1.3 and pinned identities, plus a second encrypted layer per request). Only the
  normal OpenAI-style requests are passed on; saving or loading the model's prompt files and changing
  server settings are refused on both Macs. The big Mac processes your prompts, so whoever
  administers it could see them.
- **When the big Mac is away.** Your editor gets a clear message instead of hanging: "Waking Mac
  Studio…" (the MacBook sends a Wake-on-LAN signal for 20 s), then "asleep or off" with a button to
  use this Mac's own model. If the big Mac stops answering in the middle of an answer, the answer
  ends with an error message within about 10 seconds, and the next request works as soon as the big
  Mac is back.
- **The big Mac stays in charge.** It can let paired Macs start and stop its model (on by default;
  the same memory check as a local start, and they can only stop a model they started, never while
  the big Mac is answering its own request). It stays awake while it shares on power, warns when it
  shares on battery, and shows "Shared · 1 request" while a paired Mac uses it.
- **Headless big Mac:** `slipstream-node pair | share | use | discover | trust` for a Mac mini
  without a screen. `share` forwards to a running llama-server and never starts a model by itself.
- **Version check:** a Mac with an older Slipstream gets "Update it to 0.6 or newer" instead of a
  silent failure.

## Also new

- **Share card (#DownloadMoreRAM):** the Live tab has a **Share** button that draws a picture of your
  current numbers — model, decode speed, your Mac's RAM, "Swap 0 MB" (only when it is 0) and the
  expert-cache hit rate — on your Mac. Copy it or save it as PNG. Values the app does not know are
  left out; nothing is uploaded.
- With an expert cache under 3 GiB the status pill's tooltip now says "Running on vibes and NVMe".

## How fast is it?

Measured on one M3 Pro / 36 GB with two instances (the big Mac and the MacBook roles each with their
own identity), the real engine (Groot-19B-A3B, 4 GiB cache, the app's settings) behind the
forwarder, 0 MB swap growth. Raw data in `bench/RESULTS.md` ("v0.6 M0"–"M5" and "v0.6 single-Mac
qualification").

| What we checked | Direct on the big Mac | Through the MacBook connection |
|---|---:|---:|
| Answers (plain, strict JSON, forced / automatic / required tool call) | reference | **identical** (4 of 4 runs each) |
| First word of a short answer (median, 20 pairs) | 195 ms | 191 ms (paired average **+10 ms**, within the model's own ±50 ms jitter) |
| Writing speed (256 tokens, 6 pairs) | 13.7 tok/s | **13.7 tok/s** |
| 39,515-token agent prompt: progress updates seen by the editor | 324 | **322**, plus live progress from `/slots` (read through the connection, no prompt text) |
| Stop button during a long prompt → big Mac stops | — | **0.002–0.10 s** (3 of 3) |
| Big Mac freezes mid-answer → editor gets an error | — | **9.3 s**; next request works right after it is back |
| Connect-editor key + standard streaming check | — | **passes** |

Honest limits of these numbers: both "Macs" were processes on the same Mac talking over loopback, so
they show the cost of Slipstream's own connection, not of your Wi-Fi.

Earlier milestones, with a simulated network delay (a test tool that adds ~13 ms round trip and,
in the stress test, 2 % packet loss):

- First word **+13 ms** later than direct at the 95th percentile (about one network round trip;
  target < 50 ms). Without the pre-opened spare connection it was +27 ms.
- Writing speed **+0.8 %** (95 % range −0.2 … +1.8 %, i.e. no measurable difference; target within 2 %).
- Stop button: the big Mac stops reading the prompt **5.5 ms** after the editor closes the request
  (95th percentile); on the real engine the long-prompt reader stops 21–43 ms later.
- Big Mac gone (network cut): the editor gets the error after **9 s**; reconnect **1 s** after it is back.
- Finding the big Mac on the network: under 1 s on the same Mac. Pairing: 21 ms of protocol work.
- 12 of 12 answers byte-identical with 2 % packet loss and 20 ms jitter.
- Removing a paired Mac cuts a running answer off within 0.16 s.

## Experimental: what is not tested yet

Everything above ran on **one** Mac. These need two real Macs, and we have not done that:

- the macOS **Local Network permission prompt** and the firewall with the shipped app (ad-hoc signed,
  not notarized), also after an update;
- **real Wi-Fi**: finding the other Mac on mesh routers or networks with access-point isolation,
  real Wi-Fi delay and jitter;
- **real sleep and wake**: whether Wake-on-LAN or the Sleep Proxy actually wakes a sleeping Mac, and
  a Mac really falling asleep mid-answer (simulated here by freezing the process);
- switching between Wi-Fi and Ethernet, or a VPN, while connected;
- two MacBooks on one big Mac, and a big Mac that is used locally at the same time (memory/swap on both);
- a full Kilo Code / OpenCode session over the network (tool calls, stop button).

How to use it, the privacy boundary and what to include in a report: `docs/USE_YOUR_OTHER_MAC.md`.

## Security review

An independent review of the first forwarder version found a critical request-smuggling bug (a
paired Mac could have made the big Mac's model server write prompt files) and four medium issues;
all were fixed with regression tests before pairing was built, and a fresh review of all v0.6 code
was done before this release (see Validation).

## Validation

VALIDATION_PLACEHOLDER

## Release artifacts

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `Slipstream_0.6.0_aarch64.dmg` | | |
| `Slipstream_0.6.0_aarch64.zip` | | |
| `slipstream-node_0.6.0_macos_aarch64.tar.gz` | | |

The app is ad-hoc signed and **not notarized**. The first time you open it, right-click the app
and choose Open, or run `xattr -dr com.apple.quarantine Slipstream.app`.
