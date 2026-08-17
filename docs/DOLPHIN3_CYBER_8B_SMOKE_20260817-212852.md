# Dolphin3-Cyber-8B dense Metal smoke

- Started: 2026-08-17T21:28:52+02:00
- GGUF: `/Users/schero/Modelle/dolphin3-cyber-8b-q4/Dolphin3.0-Llama3.1-8B-abliterated.Q4_K_M.gguf`
- Server: `/Applications/Slipstream.app/Contents/Resources/resources/llama-server`
- ctx=8192 ngl=99 jinja port=8080 (no --pgrn)

free+inactive before: 10.88 GiB
swap before: total = 2048.00M  used = 1165.81M  free = 882.19M  (encrypted)
owned_pid=1110
Waiting for /health…
ready after 5s · free+inactive=7.11 GiB · RSS=5.12 GiB
- chat_ok: yes
- prompt_tokens: 37
- completion_tokens: 29
- wall_s: 1.34
- decode_tok_s_approx: 21.6
- reply_preview: 'SQL Injection is an attack where an attacker injects malicious SQL code into the input fields of a web application to gain unauthorized access to data.'
# HELP llamacpp:prompt_tokens_total Number of prompt tokens processed.
# TYPE llamacpp:prompt_tokens_total counter
llamacpp:prompt_tokens_total 37
# HELP llamacpp:prompt_seconds_total Prompt process time
# TYPE llamacpp:prompt_seconds_total counter
llamacpp:prompt_seconds_total 0.255
# HELP llamacpp:prompt_tokens_cached_total Number of prompt tokens served from the KV cache instead of being processed.
# TYPE llamacpp:prompt_tokens_cached_total counter
llamacpp:prompt_tokens_cached_total 0
# HELP llamacpp:tokens_predicted_total Number of generation tokens processed.
# TYPE llamacpp:tokens_predicted_total counter
llamacpp:tokens_predicted_total 29
# HELP llamacpp:tokens_predicted_seconds_total Predict process time
# TYPE llamacpp:tokens_predicted_seconds_total counter
llamacpp:tokens_predicted_seconds_total 1.049
# HELP llamacpp:n_tokens_max Largest observed n_tokens.
# TYPE llamacpp:n_tokens_max counter
llamacpp:n_tokens_max 65
# HELP llamacpp:prompt_tokens_seconds Average prompt throughput in tokens/s.
# TYPE llamacpp:prompt_tokens_seconds gauge
- free+inactive after chat: 7.00 GiB
- RSS after chat: 5.13 GiB
- swap after: total = 2048.00M  used = 1165.81M  free = 882.19M  (encrypted)
- free+inactive after stop: 9.13 GiB
