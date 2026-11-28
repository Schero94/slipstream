# Use your other Mac (v0.6, experimental)

> **Experimental.** Tested on one Mac with two instances. Not yet tested between two real Macs
> (network permission prompt, real Wi-Fi, sleep/wake). Please tell us how it works for you:
> [GitHub issues](https://github.com/Schero94/slipstream/issues).

Your MacBook's coding editor can use the model running on a bigger Mac at home or in the office
(a Mac Studio or Mac mini). The editor keeps its address, `http://127.0.0.1:8080/v1`; the answers
come from the other Mac. Only your own Macs, paired with a 6-digit code, can connect. Nothing goes
to the internet.

![The "Your Macs" tab with the Experimental badge (demo data)](screenshots/your-macs-experimental.png)

## How to use it (app)

On the **big Mac** (the one that runs the model):

1. Start the model as usual (Model tab → Start).
2. Open **Your Macs** and turn on **Share this Mac**. A 6-digit code appears (valid 5 minutes).

On the **MacBook**:

1. Open **Your Macs** → **Find my Macs**. macOS may ask whether Slipstream may find devices on your
   local network — allow it. (This question appears only after this click, never at app start.)
2. Type the code shown on the big Mac and press Enter. The MacBook switches to the big Mac at once;
   the top bar shows "Using Mac Studio".
3. If you have not connected your editor yet: **Connect editor** → Kilo Code or OpenCode. The
   setting it writes works for both the local model and the other Mac, so switching never needs a
   new setting.

Every day after that: nothing to click. The MacBook reconnects when the app starts. If the big Mac
sleeps, the MacBook tries to wake it and tells you when it cannot reach it; "Use this Mac's model"
switches back to the local model.

Not found? Under **Not listed? Enter its address** type the address shown under the code on the big
Mac. This also works over your own VPN. There is no relay server and no NAT traversal.

## How to use it (headless, e.g. a Mac mini without a screen)

```bash
# big Mac: llama-server already runs on 127.0.0.1:8080
slipstream-node pair --show-code          # prints a code (stop `share` first, or run it next to a running share)
slipstream-node share                     # forwards to http://127.0.0.1:8080, announces on the LAN
# MacBook
slipstream-node pair --peer <fingerprint from `slipstream-node discover`> --code <6 digits>
slipstream-node use --token-file ~/.slipstream-token   # editors: http://127.0.0.1:8080/v1, API key = the file's content
```

`slipstream-node share` never starts a model by itself and never joins the community mesh.

## Privacy boundary

- **Who can connect:** only Macs you paired with a code. The code is checked with a password
  protocol (SPAKE2); a wrong code three times closes the window. Each Mac remembers the other's
  identity and refuses anyone else before reading a single request. Removing a Mac on either side
  disconnects it within a second.
- **What travels:** your prompts and the answers, encrypted between the two Macs (QUIC with
  TLS 1.3 and pinned identities, plus a second encrypted layer per request). Nothing goes to the
  internet or to us.
- **Who can read it:** the big Mac decrypts and processes your prompt — whoever administers that Mac
  could see it. Use it only with Macs you trust as much as your own.
- **What the other Mac can do:** only an allowlist of OpenAI-style requests is forwarded (chat and
  text completions, models, health, tokenize/detokenize/apply-template, a read-only `/slots` without
  prompt text, `/metrics`, a reduced `/props`). Saving or restoring the model's prompt files
  (`POST /slots/*`), changing server properties (`POST /props`) and everything else is refused on
  both Macs.
- **On the MacBook:** the forwarder listens only on `127.0.0.1` and `[::1]` and requires this
  install's local API key (the Connect-editor setting includes it). Other users and web pages on
  the MacBook cannot use it.
- **On the network:** the big Mac announces only a version, a short fingerprint and whether a pairing
  code is open (mDNS `_slipstream._udp`) — no names, no model, no RAM size. The MacBook never announces.

## What was tested — and what was not

Tested on **one Mac** (M3 Pro, 36 GB) with two instances, each with its own identity and settings
(details and raw values: `bench/RESULTS.md`, "v0.6 M0"–"M5" and "v0.6 single-Mac qualification"):

- **Same answers:** chat answers, strict JSON and tool calls (forced, automatic and "required")
  through the other-Mac connection are identical to asking the model directly.
- **Long agent prompts:** a 39,515-token agent prompt read on the big Mac shows its progress on the
  MacBook the whole time (322 progress updates; the read-only progress view `/slots` also works through the connection).
- **Stop button:** stopping a long prompt in the editor stops the big Mac within 0.1 s (3 of 3 runs).
- **Big Mac freezes mid-answer** (process paused, our stand-in for sleep): the editor gets "The other
  Mac stopped answering." after 9.3 s, new requests get a "trying to wake it" message within 2 s, and
  the next request works as soon as the big Mac is back.
- **Speed:** no measurable cost on the same Mac — first word +10 ms on average (within the model's
  own ±50 ms jitter), writing speed 13.7 tok/s in both cases. With a simulated ~13 ms network
  round trip earlier: first word +13 ms, writing speed within 1 %.
- **Editor setup:** the Connect-editor button writes exactly the key the connection requires, and the
  standard streaming check passes through it.
- 0 MB swap; pairing, wrong codes, removing a Mac, version mismatch, packet loss (2 %) and the
  wake/asleep states are covered by automated tests (304 Rust, 115 app, 9 UI checks).

**Not tested yet (needs two real Macs):**

- the macOS **Local Network permission prompt** and the firewall with the shipped (ad-hoc signed) app,
  also after an update;
- **real Wi-Fi**: finding the other Mac on mesh routers or networks with access-point isolation,
  Wi-Fi delay and jitter;
- **real sleep and wake**: whether Wake-on-LAN / the Sleep Proxy actually wakes a sleeping big Mac,
  and a Mac going to sleep in the middle of an answer (simulated here by freezing the process);
- switching between Wi-Fi and Ethernet, or a VPN, while connected;
- two MacBooks on one big Mac, and a big Mac that also works locally at the same time
  (memory and swap on both);
- a full Kilo Code / OpenCode session over the network (tool calls, the stop button).

## Tell us how it works

Please open an issue at <https://github.com/Schero94/slipstream/issues> with:

- both Macs (model, RAM, macOS version) and how they are connected (Wi-Fi / Ethernet / VPN);
- what you did and what happened (did the permission prompt appear? did "Find my Macs" see the
  other Mac? did the editor get answers? what happened when the big Mac slept?);
- the text of any message the app showed.

Please do **not** paste your prompts, your local API key (`local.token`) or the pairing code.
