# Eviction replay — 2026-11-28 (bench/fetchwall/sim_eviction.py --validate)

Traces: PGRN_TRACE, app regime (run_arm_server.sh, MTP draft 4, compact slots, io 8, lookahead on),
Qwen3.6-35B-A3B UD-Q4_K_XL from ~/Modelle, greedy, 1000 decode tokens. code = prompt_code.txt (57 tokens,
with the script's 16-token warm-up first); agent = prompt_30k.txt (39,515 tokens, no warm-up). Counts are
expert reads summed over 41 layers (40 + MTP) in the decode phase of the measured request.


## code_c4.trace  (layers 41, slots/layer 45-46)
validation clox-k4 vs engine: demand hit/miss mismatches = 0, prefetch outcomes identical
decode demand accesses: 266250
| policy | decode demand misses | decode prefetch reads | decode total reads | Δ demand vs clox-k4 | Δ total vs clox-k4 | all-phase reads | fails |
|---|---:|---:|---:|---:|---:|---:|---:|
| clox-k4 | 62777 | 82123 | 144900 | +0.0 % | +0.0 % | 157881 | 0 |
| clox-k4-nopf | 103094 | 0 | 103094 | +64.2 % | -28.9 % | 115226 | 0 |
| clox-k1 | 63536 | 83936 | 147472 | +1.2 % | +1.8 % | 160981 | 0 |
| clox-k2 | 62921 | 82991 | 145912 | +0.2 % | +0.7 % | 159166 | 0 |
| clox-k8 | 62586 | 81687 | 144273 | -0.3 % | -0.4 % | 157174 | 0 |
| lru | 63563 | 84454 | 148017 | +1.3 % | +2.2 % | 161459 | 0 |
| lfu-h4 | 94719 | 80011 | 174730 | +50.9 % | +20.6 % | 188874 | 0 |
| lfu-h8 | 93134 | 79076 | 172210 | +48.4 % | +18.8 % | 186022 | 0 |
| lfu-h16 | 92057 | 78771 | 170828 | +46.6 % | +17.9 % | 184481 | 0 |
| lfu-h32 | 91222 | 78654 | 169876 | +45.3 % | +17.2 % | 183460 | 0 |
| lfu-h64 | 90913 | 78820 | 169733 | +44.8 % | +17.1 % | 183288 | 0 |
| lfu-h256 | 91055 | 79224 | 170279 | +45.0 % | +17.5 % | 183818 | 0 |
| arc | 71084 | 84152 | 155236 | +13.2 % | +7.1 % | 167880 | 0 |
| clox-k4+lp | 55295 | 77122 | 132417 | -11.9 % | -8.6 % | 145284 | 0 |
| clox-k8+lp | 55132 | 76639 | 131771 | -12.2 % | -9.1 % | 144554 | 0 |
| lru+lp | 56255 | 80336 | 136591 | -10.4 % | -5.7 % | 149954 | 0 |
| lfu-h4+lp | 54435 | 75849 | 130284 | -13.3 % | -10.1 % | 143191 | 0 |
| lfu-h16+lp | 54473 | 74290 | 128763 | -13.2 % | -11.1 % | 141244 | 0 |
| lfu-h64+lp | 54607 | 73828 | 128435 | -13.0 % | -11.4 % | 140819 | 0 |
| lfu-h256+lp | 54706 | 74028 | 128734 | -12.9 % | -11.2 % | 141102 | 0 |
| arc+lp | 55717 | 78193 | 133910 | -11.2 % | -7.6 % | 146309 | 0 |
| belady | 39642 | 46234 | 85876 | -36.9 % | -40.7 % | 94958 | 0 |
| belady-nopf | 70642 | 0 | 70642 | +12.5 % | -51.2 % | 79392 | 0 |


## agent_c4.trace  (layers 41, slots/layer 45-46)
validation clox-k4 vs engine: demand hit/miss mismatches = 0, prefetch outcomes identical
decode demand accesses: 330273
| policy | decode demand misses | decode prefetch reads | decode total reads | Δ demand vs clox-k4 | Δ total vs clox-k4 | all-phase reads | fails |
|---|---:|---:|---:|---:|---:|---:|---:|
| clox-k4 | 99217 | 91981 | 191198 | +0.0 % | +0.0 % | 367210 | 0 |
| clox-k4-nopf | 142458 | 0 | 142458 | +43.6 % | -25.5 % | 318487 | 0 |
| clox-k1 | 100625 | 94405 | 195030 | +1.4 % | +2.0 % | 371282 | 0 |
| clox-k2 | 99565 | 92958 | 192523 | +0.4 % | +0.7 % | 368586 | 0 |
| clox-k8 | 99163 | 91192 | 190355 | -0.1 % | -0.4 % | 366298 | 0 |
| lru | 100782 | 95284 | 196066 | +1.6 % | +2.5 % | 372309 | 0 |
| lfu-h4 | 131279 | 86769 | 218048 | +32.3 % | +14.0 % | 394193 | 0 |
| lfu-h8 | 129177 | 85707 | 214884 | +30.2 % | +12.4 % | 391139 | 0 |
| lfu-h16 | 127201 | 85037 | 212238 | +28.2 % | +11.0 % | 388358 | 0 |
| lfu-h32 | 125699 | 84631 | 210330 | +26.7 % | +10.0 % | 386166 | 0 |
| lfu-h64 | 124996 | 84393 | 209389 | +26.0 % | +9.5 % | 384846 | 0 |
| lfu-h256 | 124812 | 84535 | 209347 | +25.8 % | +9.5 % | 382940 | 0 |
| arc | 106659 | 95382 | 202041 | +7.5 % | +5.7 % | 364411 | 0 |
| clox-k4+lp | 90183 | 85357 | 175540 | -9.1 % | -8.2 % | 351552 | 0 |
| clox-k8+lp | 89984 | 84660 | 174644 | -9.3 % | -8.7 % | 350587 | 0 |
| lru+lp | 91554 | 89582 | 181136 | -7.7 % | -5.3 % | 357379 | 0 |
| lfu-h4+lp | 89029 | 82757 | 171786 | -10.3 % | -10.2 % | 347931 | 0 |
| lfu-h16+lp | 88839 | 81098 | 169937 | -10.5 % | -11.1 % | 346057 | 0 |
| lfu-h64+lp | 88758 | 80453 | 169211 | -10.5 % | -11.5 % | 344668 | 0 |
| lfu-h256+lp | 88657 | 80483 | 169140 | -10.6 % | -11.5 % | 342733 | 0 |
| arc+lp | 88486 | 86778 | 175264 | -10.8 % | -8.3 % | 337634 | 0 |
| belady | 69029 | 52066 | 121095 | -30.4 % | -36.7 % | 265846 | 0 |
| belady-nopf | 102908 | 0 | 102908 | +3.7 % | -46.2 % | 247656 | 0 |


## code_c10.trace  (layers 41, slots/layer 113-114)
validation clox-k4 vs engine: demand hit/miss mismatches = 0, prefetch outcomes identical
decode demand accesses: 266250
| policy | decode demand misses | decode prefetch reads | decode total reads | Δ demand vs clox-k4 | Δ total vs clox-k4 | all-phase reads | fails |
|---|---:|---:|---:|---:|---:|---:|---:|
| clox-k4 | 17823 | 32528 | 50351 | +0.0 % | +0.0 % | 56615 | 0 |
| clox-k4-nopf | 33463 | 0 | 33463 | +87.8 % | -33.5 % | 39446 | 0 |
| clox-k1 | 19029 | 35080 | 54109 | +6.8 % | +7.5 % | 60599 | 0 |
| clox-k2 | 18271 | 33788 | 52059 | +2.5 % | +3.4 % | 58410 | 0 |
| clox-k8 | 17710 | 31822 | 49532 | -0.6 % | -1.6 % | 55784 | 0 |
| lru | 18514 | 34144 | 52658 | +3.9 % | +4.6 % | 59030 | 0 |
| lfu-h4 | 29988 | 35757 | 65745 | +68.3 % | +30.6 % | 72155 | 0 |
| lfu-h8 | 28965 | 34794 | 63759 | +62.5 % | +26.6 % | 70118 | 0 |
| lfu-h16 | 27809 | 33741 | 61550 | +56.0 % | +22.2 % | 67867 | 0 |
| lfu-h32 | 27496 | 33426 | 60922 | +54.3 % | +21.0 % | 67234 | 0 |
| lfu-h64 | 27957 | 33896 | 61853 | +56.9 % | +22.8 % | 68192 | 0 |
| lfu-h256 | 29077 | 34969 | 64046 | +63.1 % | +27.2 % | 70424 | 0 |
| arc | 19477 | 34290 | 53767 | +9.3 % | +6.8 % | 60092 | 0 |
| clox-k4+lp | 16185 | 31984 | 48169 | -9.2 % | -4.3 % | 54403 | 0 |
| clox-k8+lp | 16203 | 31200 | 47403 | -9.1 % | -5.9 % | 53625 | 0 |
| lru+lp | 16577 | 34119 | 50696 | -7.0 % | +0.7 % | 57039 | 0 |
| lfu-h4+lp | 15586 | 34483 | 50069 | -12.6 % | -0.6 % | 56387 | 0 |
| lfu-h16+lp | 15548 | 32287 | 47835 | -12.8 % | -5.0 % | 54064 | 0 |
| lfu-h64+lp | 16458 | 32049 | 48507 | -7.7 % | -3.7 % | 54758 | 0 |
| lfu-h256+lp | 17187 | 32776 | 49963 | -3.6 % | -0.8 % | 56253 | 0 |
| arc+lp | 16003 | 34447 | 50450 | -10.2 % | +0.2 % | 56750 | 0 |
| belady | 7032 | 13155 | 20187 | -60.5 % | -59.9 % | 25511 | 0 |
| belady-nopf | 16446 | 0 | 16446 | -7.7 % | -67.3 % | 21601 | 0 |


## agent_c10.trace  (layers 41, slots/layer 113-114)
validation clox-k4 vs engine: demand hit/miss mismatches = 0, prefetch outcomes identical
decode demand accesses: 330273
| policy | decode demand misses | decode prefetch reads | decode total reads | Δ demand vs clox-k4 | Δ total vs clox-k4 | all-phase reads | fails |
|---|---:|---:|---:|---:|---:|---:|---:|
| clox-k4 | 41126 | 35539 | 76665 | +0.0 % | +0.0 % | 240807 | 0 |
| clox-k4-nopf | 56704 | 0 | 56704 | +37.9 % | -26.0 % | 220921 | 0 |
| clox-k1 | 42781 | 38545 | 81326 | +4.0 % | +6.1 % | 245523 | 0 |
| clox-k2 | 41927 | 36916 | 78843 | +1.9 % | +2.8 % | 243023 | 0 |
| clox-k8 | 40955 | 35090 | 76045 | -0.4 % | -0.8 % | 240186 | 0 |
| lru | 42181 | 37462 | 79643 | +2.6 % | +3.9 % | 244161 | 0 |
| lfu-h4 | 53475 | 38549 | 92024 | +30.0 % | +20.0 % | 256648 | 0 |
| lfu-h8 | 51999 | 37138 | 89137 | +26.4 % | +16.3 % | 253806 | 0 |
| lfu-h16 | 50606 | 35684 | 86290 | +23.1 % | +12.6 % | 251093 | 0 |
| lfu-h32 | 49597 | 34921 | 84518 | +20.6 % | +10.2 % | 248781 | 0 |
| lfu-h64 | 49238 | 34967 | 84205 | +19.7 % | +9.8 % | 247204 | 0 |
| lfu-h256 | 48879 | 35471 | 84350 | +18.9 % | +10.0 % | 243109 | 0 |
| arc | 40351 | 40948 | 81299 | -1.9 % | +6.0 % | 217037 | 0 |
| clox-k4+lp | 39310 | 34771 | 74081 | -4.4 % | -3.4 % | 238223 | 0 |
| clox-k8+lp | 39235 | 34232 | 73467 | -4.6 % | -4.2 % | 237608 | 0 |
| lru+lp | 39915 | 37447 | 77362 | -2.9 % | +0.9 % | 241880 | 0 |
| lfu-h4+lp | 38309 | 37262 | 75571 | -6.8 % | -1.4 % | 240195 | 0 |
| lfu-h16+lp | 37816 | 34522 | 72338 | -8.0 % | -5.6 % | 237141 | 0 |
| lfu-h64+lp | 37789 | 33609 | 71398 | -8.1 % | -6.9 % | 234397 | 0 |
| lfu-h256+lp | 37412 | 33925 | 71337 | -9.0 % | -6.9 % | 230096 | 0 |
| arc+lp | 35654 | 42295 | 77949 | -13.3 % | +1.7 % | 213687 | 0 |
| belady | 20927 | 12710 | 33637 | -49.1 % | -56.1 % | 129809 | 0 |
| belady-nopf | 29535 | 0 | 29535 | -28.2 % | -61.5 % | 125704 | 0 |

## Engine cross-check (engine trace vs replay of another policy's trace)

- code_c4_lp.trace: engine decode demand misses 55245, prefetch reads 77131; sim clox-k4+lp 55245; per-access hit/miss mismatches 0
- code_c4_lp.trace: engine decode demand misses 55245, prefetch reads 77131; sim clox-k4 62718; per-access hit/miss mismatches 15586
- code_c4_decay.trace: engine decode demand misses 54473, prefetch reads 74281; sim lfu-h16+lp 54474; per-access hit/miss mismatches 23
- code_c4.trace: engine decode demand misses 62777, prefetch reads 82123; sim clox-k4+lp 55295; per-access hit/miss mismatches 15542
- code_c4.trace: engine decode demand misses 62777, prefetch reads 82123; sim lfu-h16+lp 54473; per-access hit/miss mismatches 22539
