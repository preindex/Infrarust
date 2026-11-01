# Floodgate handshake fixture

`floodgate_handshake_v0.json` is a **synthetic, deterministic** AES-GCM
differential fixture. It is not captured from a live Geyser or Floodgate
instance and contains no real player data. The key and IV are test-only.

- Sources and framing: see [PROVENANCE.txt](PROVENANCE.txt).
- Regenerate: `python tools/generate_floodgate_fixture.py`
- Independently verify: `python tools/verify_floodgate_fixture.py`
  (needs `cryptography`; run without `-I` so the user site-packages are visible).
