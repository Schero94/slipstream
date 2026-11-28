#!/usr/bin/env python3
"""Offline expert-eviction replay for the Slipstream PGRN cache.

Replays a PGRN_TRACE cache-event trace (see peregrine_stream.h) through per-layer
caches with the engine's real slot counts ("C layer cap" lines) and counts reads for
several eviction policies. Pins are replayed exactly (a demand-accessed expert stays
pinned until the layer's next batch begin; prefetches may not evict pinned slots), and
lookahead prefetch requests ("L" lines, recorded before the residency filter) are
re-filtered against each policy's own residency, so every policy sees the same demand
stream and the same predictor output. Eviction never changes routing, so the demand
stream is exact for every policy (lossless).

Policies:
  clox-k<K>   faithful copy of src/peregrine_cache.c (CLOCK-LRU-K, ring + hand + pins);
              K=4 is the engine default and is validated event-by-event against the
              hit/miss and prefetch outcomes recorded in the trace.
  lru         least recently used (prefetch insert = most recent use).
  lfu-h<H>    lowest exponentially decayed demand count (arXiv 2611.15842 App. C / Colibri
              qwen36_expected_policy.h), half-life H ubatches, history kept across
              evictions, ties -> LRU.
  arc         ARC (Megiddo & Modha) with pins; prefetch inserts into T1.
  belady      farthest next demand use (prefetch bypassed if it would evict something
              needed sooner); clairvoyant upper bound with the same prefetch stream.
  belady-nopf demand-only Belady (the paper's bound; prefetch ignored).
  <p>-nopf    the same policy with lookahead prefetch ignored (demand only).
  <p>+kf      kick-free (PGRN_KICKFREE=1): a lookahead kick first releases the layer's pins
              of the previous ubatch (they no longer block its prefetches); combinable,
              e.g. lfu-h16+lp+kf.
  <p>+lp      lookahead-protect: during a lookahead kick, the predicted experts that are
              already resident, and the ones it has just loaded, are pinned until the layer's
              next batch begin (engine semantics), so the kick's own prefetches cannot
              evict them.

Reads: a prefetch whose publish fails (no evictable slot) has already been read from SSD;
the engine logs it as 'P layer expert 2' (3 = the read itself failed). The replay counts these
as "wasted prefetch reads" and includes them in total reads.

--engine-summary prints, per trace (= per arm), the engine's own recorded outcomes: demand
misses, prefetch reads published / wasted on a failed publish (P 2) / failed reads (P 3), and
the wasted rate, for the decode phase and the whole run.

usage: sim_eviction.py TRACE [TRACE ...]  [--policies a,b,...] [--validate] [--engine-summary]
"""
import argparse
import collections
import math
import sys

INF = float("inf")


# ----------------------------------------------------------------------------- parsing
def parse(path):
    caps = {}
    ev = collections.defaultdict(list)   # layer -> [(tag, expert, flag)]
    with open(path) as f:
        for line in f:
            p = line.split()
            if not p:
                continue
            t = p[0]
            if t == "C":
                caps[int(p[1])] = int(p[2])
            elif t == "B":
                ev[int(p[1])].append(("B", -1, -1))
            elif t == "D":
                ev[int(p[1])].append(("D", int(p[2]), int(p[3])))
            elif t == "N":
                ev[int(p[1])].append(("N", int(p[2]), int(p[3])))
            elif t == "L":
                ev[int(p[1])].append(("L", int(p[2]), -1))
            elif t == "P":
                ev[int(p[1])].append(("P", int(p[2]), int(p[3])))
    return caps, ev


def decode_start_index(events):
    """Index of the first event of the decode phase of the LAST request.

    Groups = demand accesses between batch begins. Prefill groups are either > 40
    distinct experts (one window) or runs of >= 4 consecutive full io windows (8).
    Decode starts after the last prefill group found in the first 60 % of groups."""
    groups = []   # (start_event_index, size)
    cur = None
    for i, (t, _, _) in enumerate(events):
        if t == "B":
            if cur is not None and cur[1] > 0:
                groups.append(tuple(cur))
            cur = [i, 0]
        elif t == "D" and cur is not None:
            cur[1] += 1
    if cur is not None and cur[1] > 0:
        groups.append(tuple(cur))
    if not groups:
        return 0
    limit = int(len(groups) * 0.6)
    last = -1
    run = 0
    for gi, (_, sz) in enumerate(groups[:limit]):
        run = run + 1 if sz == 8 else 0
        if sz > 40 or run >= 4:
            last = gi
    if last < 0:
        return 0
    nxt = last + 1
    if nxt < len(groups) and groups[nxt][1] < 8 and groups[last][1] == 8:
        nxt += 1   # trailing partial io window of the prefill
    return groups[nxt][0] if nxt < len(groups) else len(events)


# ----------------------------------------------------------------------------- policies
class Clox:
    """Exact port of peregrine_cache.c (pgr_clox_*)."""

    def __init__(self, cap, k=4):
        self.cap, self.k = cap, k
        self.keys, self.ctr, self.pin = [None] * cap, [0] * cap, [0] * cap
        self.count = 0
        self.hand = 0
        self.where = {}

    def resident(self, e):
        return e in self.where

    def access(self, e, t):   # lookup with frequency bump; True on hit
        s = self.where.get(e)
        if s is None:
            return False
        if self.ctr[s] < self.k:
            self.ctr[s] += 1
        return True

    def insert(self, e, t, prefetch=False):
        if self.count < self.cap:
            s = self.count
            self.keys[s], self.ctr[s] = e, 1
            self.count += 1
            self.where[e] = s
            return True
        if sum(self.pin) == self.cap:
            return False
        while True:
            h = self.hand
            if self.pin[h]:
                self.hand = (h + 1) % self.cap
                continue
            if self.ctr[h] > 0:
                self.ctr[h] -= 1
                self.hand = (h + 1) % self.cap
            else:
                del self.where[self.keys[h]]
                self.keys[h], self.ctr[h] = e, 1
                self.where[e] = h
                self.hand = (h + 1) % self.cap
                return True

    def pin_key(self, e):
        self.pin[self.where[e]] = 1

    def save_pins(self):
        return list(self.pin)

    def restore_pins(self, saved):
        self.pin = saved

    def unpin_all(self):
        self.pin = [0] * self.cap


class Lru:
    def __init__(self, cap):
        self.cap = cap
        self.od = collections.OrderedDict()
        self.pinned = set()

    def resident(self, e):
        return e in self.od

    def access(self, e, t):
        if e in self.od:
            self.od.move_to_end(e)
            return True
        return False

    def insert(self, e, t, prefetch=False):
        if len(self.od) >= self.cap:
            for v in self.od:
                if v not in self.pinned:
                    break
            else:
                return False
            del self.od[v]
        self.od[e] = True
        return True

    def pin_key(self, e):
        self.pinned.add(e)

    def save_pins(self):
        return set(self.pinned)

    def restore_pins(self, saved):
        self.pinned = saved

    def unpin_all(self):
        self.pinned.clear()


class LfuDecay:
    def __init__(self, cap, half_life):
        self.cap = cap
        self.lam = math.log(2) / half_life
        self.score = {}   # expert -> (value, t_ref)   (global history)
        self.res = {}     # expert -> last use time (for LRU tie break)
        self.pinned = set()

    def _val(self, e, t):
        v = self.score.get(e)
        if v is None:
            return 0.0
        return v[0] * math.exp(-self.lam * (t - v[1]))

    def resident(self, e):
        return e in self.res

    def access(self, e, t):
        self.score[e] = (self._val(e, t) + 1.0, t)
        if e in self.res:
            self.res[e] = t
            return True
        return False

    def insert(self, e, t, prefetch=False):
        if len(self.res) >= self.cap:
            best, bk = None, None
            for v, lt in self.res.items():
                if v in self.pinned:
                    continue
                k = (self._val(v, t), lt)
                if bk is None or k < bk:
                    best, bk = v, k
            if best is None:
                return False
            del self.res[best]
        self.res[e] = t
        return True

    def pin_key(self, e):
        self.pinned.add(e)

    def save_pins(self):
        return set(self.pinned)

    def restore_pins(self, saved):
        self.pinned = saved

    def unpin_all(self):
        self.pinned.clear()


class Arc:
    def __init__(self, cap):
        self.c = cap
        self.p = 0.0
        self.t1, self.t2 = collections.OrderedDict(), collections.OrderedDict()
        self.b1, self.b2 = collections.OrderedDict(), collections.OrderedDict()
        self.pinned = set()

    def resident(self, e):
        return e in self.t1 or e in self.t2

    def access(self, e, t):
        if e in self.t1:
            del self.t1[e]
            self.t2[e] = True
            return True
        if e in self.t2:
            self.t2.move_to_end(e)
            return True
        return False

    def _lru_unpinned(self, od):
        for v in od:
            if v not in self.pinned:
                return v
        return None

    def _replace(self, e):
        v1 = self._lru_unpinned(self.t1)
        v2 = self._lru_unpinned(self.t2)
        use_t1 = len(self.t1) > 0 and (len(self.t1) > self.p or (e in self.b2 and len(self.t1) == int(self.p)))
        if use_t1 and v1 is not None or v2 is None:
            if v1 is None:
                return False
            del self.t1[v1]
            self.b1[v1] = True
        else:
            del self.t2[v2]
            self.b2[v2] = True
        return True

    def insert(self, e, t, prefetch=False):
        c = self.c
        if not prefetch and e in self.b1:
            self.p = min(c, self.p + max(len(self.b2) / max(len(self.b1), 1), 1))
            if len(self.t1) + len(self.t2) >= c and not self._replace(e):
                return False
            del self.b1[e]
            self.t2[e] = True
            return True
        if not prefetch and e in self.b2:
            self.p = max(0.0, self.p - max(len(self.b1) / max(len(self.b2), 1), 1))
            if len(self.t1) + len(self.t2) >= c and not self._replace(e):
                return False
            del self.b2[e]
            self.t2[e] = True
            return True
        self.b1.pop(e, None)
        self.b2.pop(e, None)
        l1 = len(self.t1) + len(self.b1)
        if l1 >= c:
            if len(self.t1) < c and self.b1:
                self.b1.popitem(last=False)
                if len(self.t1) + len(self.t2) >= c and not self._replace(e):
                    return False
            else:
                v = self._lru_unpinned(self.t1)
                if v is None:
                    if len(self.t1) + len(self.t2) >= c and not self._replace(e):
                        return False
                else:
                    del self.t1[v]
        elif len(self.t1) + len(self.t2) >= c:
            if l1 + len(self.t2) + len(self.b2) >= 2 * c and self.b2:
                self.b2.popitem(last=False)
            if not self._replace(e):
                return False
        self.t1[e] = True
        return True

    def pin_key(self, e):
        self.pinned.add(e)

    def save_pins(self):
        return set(self.pinned)

    def restore_pins(self, saved):
        self.pinned = saved

    def unpin_all(self):
        self.pinned.clear()


class Belady:
    def __init__(self, cap, next_use):
        self.cap = cap
        self.next_use = next_use   # callable(expert, event_index) -> next demand index
        self.res = set()
        self.pinned = set()
        self.idx = 0

    def resident(self, e):
        return e in self.res

    def access(self, e, t):
        return e in self.res

    def insert(self, e, t, prefetch=False):
        if len(self.res) >= self.cap:
            best, bn = None, -1
            for v in self.res:
                if v in self.pinned:
                    continue
                n = self.next_use(v, self.idx)
                if n > bn:
                    best, bn = v, n
            if best is None:
                return False
            if prefetch and self.next_use(e, self.idx) >= bn:
                return False   # bypass: the prefetched expert is needed later than any victim
            self.res.discard(best)
        self.res.add(e)
        return True

    def pin_key(self, e):
        self.pinned.add(e)

    def save_pins(self):
        return set(self.pinned)

    def restore_pins(self, saved):
        self.pinned = saved

    def unpin_all(self):
        self.pinned.clear()


# ----------------------------------------------------------------------------- replay
def make_next_use(events):
    pos = collections.defaultdict(list)
    for i, (t, e, _) in enumerate(events):
        if t == "D":
            pos[e].append(i)
    ptr = collections.defaultdict(int)

    def next_use(e, i):
        lst = pos.get(e)
        if not lst:
            return INF
        j = ptr[e]
        while j < len(lst) and lst[j] <= i:
            j += 1
        ptr[e] = j
        return lst[j] if j < len(lst) else INF
    return next_use


def replay(events, pol, dstart, use_prefetch=True, validate=False, protect=False, kickfree=False):
    """Returns dict of counts (all / decode): demand misses, prefetch reads (published),
    prefetch reads wasted on a failed publish."""
    c = dict(dm_all=0, pf_all=0, dm_dec=0, pf_dec=0, acc_dec=0, mism=0, fail=0, pw_all=0, pw_dec=0)
    t = 0
    new_group = False
    kick = []   # one lookahead kick = a run of L lines; residency filtered up front (as kick_list)

    def flush_kick(dec):
        if kickfree:
            pol.unpin_all()   # PGRN_KICKFREE: batch_begin(layer) before the kick
        cold = [x for x in kick if not pol.resident(x)]
        if protect:
            # lookahead-protect: the predicted experts that are already resident may not be
            # evicted by this kick's own prefetches (they are about to be used). As in the
            # engine, these pins last until the layer's next batch begin.
            for x in kick:
                if pol.resident(x):
                    pol.pin_key(x)
        kick.clear()
        for x in cold:
            if pol.resident(x):
                continue
            if pol.insert(x, t, prefetch=True):
                c["pf_all"] += 1
                if dec:
                    c["pf_dec"] += 1
                if protect:
                    pol.pin_key(x)   # nor may a later prefetch of the same kick evict it
            else:
                c["pw_all"] += 1     # read from SSD, then dropped: no evictable slot
                if dec:
                    c["pw_dec"] += 1
    def demand(e, flag, dec, hit):
        if not hit:
            if not pol.insert(e, t):
                c["fail"] += 1
            c["dm_all"] += 1
            if dec:
                c["dm_dec"] += 1
        if dec:
            c["acc_dec"] += 1
        if validate and int(hit) != flag:
            c["mism"] += 1
        if pol.resident(e):
            pol.pin_key(e)

    n = len(events)
    i = 0
    while i < n:
        tag, e, flag = events[i]
        if isinstance(pol, Belady):
            pol.idx = i
        dec = i >= dstart
        if kick and tag != "L":
            flush_kick(dec)
        if tag == "B":
            pol.unpin_all()
            new_group = True
            i += 1
        elif tag in ("D", "N"):
            # one contiguous run of demand accesses (a get_many window or slot resolves)
            j = i
            while j < n and events[j][0] in ("D", "N"):
                j += 1
            run = events[i:j]
            keys = [(x[1], x[2]) for x in run if x[0] == "D"]
            batch = all(x[2] == 2 for x in run if x[0] == "N")
            if keys and new_group:
                t += 1
                new_group = False
            if isinstance(pol, Belady):
                pol.idx = j - 1
            if batch:
                # pgr_stream_get_many: phase 1 resolves (and pins) the policy's own hits,
                # phase 2 publishes the misses in request order.
                misses = []
                for e2, f2 in keys:
                    if pol.resident(e2):
                        demand(e2, f2, dec, pol.access(e2, t))
                    else:
                        misses.append((e2, f2))
                for e2, f2 in misses:
                    demand(e2, f2, dec, pol.access(e2, t))
            else:
                for e2, f2 in keys:
                    demand(e2, f2, dec, pol.access(e2, t))
            i = j
        else:
            if tag == "L" and use_prefetch:
                kick.append(e)
            i += 1
    if kick:
        flush_kick(True)
    return c


def validate_prefetch(events, cap):
    """Replay with the engine policy and compare prefetch outcomes with 'P' lines."""
    pol = Clox(cap, 4)
    want = []   # engine outcomes in order: 1 published, 2 no slot
    got = []
    t = 0
    kick = []
    for tag, e, flag in events:
        if kick and tag != "L":
            for x in [x for x in kick if not pol.resident(x)]:
                got.append(1 if pol.insert(x, t, prefetch=True) else 2)
            kick = []
        if tag == "B":
            pol.unpin_all()
        elif tag == "D":
            if not pol.access(e, t):
                pol.insert(e, t)
            pol.pin_key(e)
        elif tag == "L":
            kick.append(e)
        elif tag == "P" and flag in (1, 2):
            want.append(flag)
    return want == got, len(want), len(got)


def engine_summary(path, ev):
    """The engine's recorded outcomes ('D' hit flag, 'P' result) of one trace."""
    tot = {"dec": collections.Counter(), "all": collections.Counter()}
    for l, events in ev.items():
        ds = decode_start_index(events)
        for i, (t, _, f) in enumerate(events):
            keys = ("all", "dec") if i >= ds else ("all",)
            for k in keys:
                if t == "D":
                    tot[k]["acc"] += 1
                    tot[k]["dmiss"] += f == 0
                elif t == "P":
                    tot[k][f"p{f}"] += 1
    print(f"\n## engine outcomes: {path}")
    print("| phase | demand accesses | demand misses | prefetch published (P1) | wasted: no slot (P2) | read failed (P3) "
          "| wasted rate (P2+P3)/(P1+P2+P3) | total SSD reads (miss+P1+P2+P3) |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for k in ("dec", "all"):
        r = tot[k]
        pr = r["p1"] + r["p2"] + r["p3"]
        rate = f"{100.0 * (r['p2'] + r['p3']) / pr:.2f} %" if pr else "-"
        print(f"| {'decode' if k == 'dec' else 'all'} | {r['acc']} | {r['dmiss']} | {r['p1']} | {r['p2']} | {r['p3']} | {rate} "
              f"| {r['dmiss'] + pr} |")


ALL = ["clox-k4", "clox-k4-nopf", "clox-k1", "clox-k2", "clox-k8", "lru", "lfu-h4", "lfu-h8", "lfu-h16",
       "lfu-h32", "lfu-h64", "lfu-h256", "arc", "clox-k4+lp", "clox-k8+lp", "lru+lp", "lfu-h4+lp", "lfu-h16+lp", "lfu-h64+lp",
       "lfu-h256+lp", "arc+lp", "clox-k4+kf", "clox-k4+lp+kf", "lfu-h16+lp+kf",
       "belady", "belady-nopf"]


def build(name, cap, events):
    if name.endswith("-nopf") and name != "belady-nopf":
        return build(name[:-5], cap, events)[0], False
    if name.endswith("+lp") or name.endswith("+kf"):
        pol, pf = build(name[:-3], cap, events)
        return pol, pf
    if name.startswith("clox-k"):
        return Clox(cap, int(name[6:])), True
    if name == "lru":
        return Lru(cap), True
    if name.startswith("lfu-h"):
        return LfuDecay(cap, float(name[5:])), True
    if name == "arc":
        return Arc(cap), True
    if name == "belady":
        return Belady(cap, make_next_use(events)), True
    if name == "belady-nopf":
        return Belady(cap, make_next_use(events)), False
    raise ValueError(name)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("traces", nargs="+")
    ap.add_argument("--policies", default=",".join(ALL))
    ap.add_argument("--validate", action="store_true")
    ap.add_argument("--engine-summary", action="store_true")
    a = ap.parse_args()
    pols = a.policies.split(",")
    for path in a.traces:
        caps, ev = parse(path)
        if a.engine_summary:
            engine_summary(path, ev)
            continue
        layers = sorted(l for l in ev if l in caps)
        tot = {p: collections.Counter() for p in pols}
        val_ok = True
        mism = 0
        for l in layers:
            events = ev[l]
            ds = decode_start_index(events)
            if a.validate:
                ok, nw, ng = validate_prefetch(events, caps[l])
                if not ok:
                    val_ok = False
                    print(f"  layer {l}: prefetch outcome mismatch ({nw} engine vs {ng} sim)", file=sys.stderr)
            for p in pols:
                pol, pf = build(p, caps[l], events)
                r = replay(events, pol, ds, use_prefetch=pf, validate=(a.validate and p == "clox-k4"),
                           protect="+lp" in p, kickfree="+kf" in p)
                tot[p].update(r)
                if p == "clox-k4":
                    mism += r["mism"]
        capstr = f"{min(caps.values())}-{max(caps.values())}" if caps else "?"
        print(f"\n## {path}  (layers {len(layers)}, slots/layer {capstr})")
        if a.validate:
            print(f"validation clox-k4 vs engine: demand hit/miss mismatches = {mism}, "
                  f"prefetch outcomes {'identical' if val_ok else 'DIFFER'}")
        base = tot["clox-k4"] if "clox-k4" in tot else None
        acc = next(iter(tot.values()))["acc_dec"]
        print(f"decode demand accesses: {acc}")
        print("| policy | decode demand misses | decode prefetch reads | decode wasted prefetch reads (rate) "
              "| decode total reads | Δ demand vs clox-k4 | Δ total vs clox-k4 | all-phase reads | fails |")
        print("|---|---:|---:|---:|---:|---:|---:|---:|---:|")
        for p in pols:
            r = tot[p]
            dm, pw = r["dm_dec"], r["pw_dec"]
            pf = r["pf_dec"] + pw   # wasted reads are SSD reads too
            wr = f"{pw} ({100.0 * pw / pf:.2f} %)" if pf else "0"
            if base:
                bd, bt = base["dm_dec"], base["dm_dec"] + base["pf_dec"] + base["pw_dec"]
                d1 = f"{100.0 * (dm - bd) / bd:+.1f} %" if bd else "-"
                d2 = f"{100.0 * (dm + pf - bt) / bt:+.1f} %" if bt else "-"
            else:
                d1 = d2 = "-"
            print(f"| {p} | {dm} | {r['pf_dec']} | {wr} | {dm + pf} | {d1} | {d2} | {r['dm_all'] + r['pf_all'] + r['pw_all']} | {r['fail']} |")


if __name__ == "__main__":
    main()
