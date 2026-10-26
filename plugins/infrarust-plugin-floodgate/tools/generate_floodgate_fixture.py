#!/usr/bin/env python3
"""Generate the SYNTHETIC Floodgate handshake fixture (deterministic).

Nothing here is captured from a live Geyser/Floodgate instance. The key, IV,
player fields and XUID are all test-only values. Framing mirrors the Geyser
`common` module sources (see tests/fixtures/PROVENANCE.txt):

  * BedrockData.toString(): 12 fields joined by NUL, in declaration order.
  * FloodgateCipher.HEADER: "^Floodgate^" + (char)(VERSION + 0x3E) => "^Floodgate^>".
  * AesCipher.encrypt(): AES/GCM/NoPadding, 12-byte IV, 128-bit tag appended to
    the ciphertext by the JCE, no AAD. With Base64Topping the wire form is
      HEADER + b64(iv) + '!' (0x21) + b64(ciphertext || tag)

Usage:
  python tools/generate_floodgate_fixture.py [OUTPUT_JSON]

Requires: cryptography (pip install cryptography). Run from any directory.
"""

import base64
import json
import pathlib
import sys
import uuid

from cryptography.hazmat.primitives.ciphers.aead import AESGCM

# --- framing constants (from FloodgateCipher / BedrockData) -----------------
IDENTIFIER = "^Floodgate^"
VERSION = 0
HEADER = IDENTIFIER + chr(VERSION + 0x3E)  # "^Floodgate^>"
SPLITTER = "!"  # 0x21, emitted after the base64 IV
PROTOCOL_HOST_MAX = 255  # Minecraft Handshake server-address limit (wiki.vg)
EXPECTED_LENGTH = 12

# --- TEST-ONLY key material (NOT a real secret) -----------------------------
KEY = bytes(range(0x00, 0x10))  # 00 01 .. 0f  (16 bytes, AES-128)
IV = bytes(range(0xA0, 0xAC))  # a0 a1 .. ab  (12 bytes)

# --- synthetic player data (no real account) --------------------------------
FIELDS = [
    ("version", "synthetic-fixture-1"),
    ("username", "Bedrock User"),
    ("xuid", "2535400000000001"),
    ("deviceOs", "1"),
    ("languageCode", "en_US"),
    ("uiProfile", "0"),
    ("inputMode", "1"),
    ("ip", "198.51.100.7"),  # TEST-NET-2 documentation range
    ("linkedPlayer", "null"),  # literal "null" when unlinked
    ("fromProxy", "0"),
    ("subscribeId", "0"),
    ("verifyCode", "synthetic-verify"),
]

HOSTNAME = "play.example.test"
USERNAME_PREFIX = "."  # Floodgate config.yml default: username-prefix "."
REPLACE_SPACES = True  # Floodgate config.yml default: replace-spaces true


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


def encrypt_handshake_data(plaintext: str) -> str:
    """Produce the Floodgate wire string for `plaintext` (AesCipher + Base64Topping)."""
    sealed = AESGCM(KEY).encrypt(IV, plaintext.encode("utf-8"), None)  # ct || tag, no AAD
    ciphertext, tag = sealed[:-16], sealed[-16:]
    assert len(tag) == 16
    return HEADER + b64(IV) + SPLITTER + b64(ciphertext + tag)


def raw_handshake_host(hostname: str, wire: str) -> str:
    # Floodgate's separateHostname() splits the raw host on NUL and finds the marker segment.
    return hostname + "\0" + wire


def expected_username(raw: str) -> str:
    name = raw.replace(" ", "_") if REPLACE_SPACES else raw
    return USERNAME_PREFIX + name


def build_length_cases(wire: str) -> dict:
    """Length calculator inputs/outputs for short and near-limit hostnames.

    The Floodgate data is independent of the hostname, so the appended tail is
    NUL + wire. Total Java-string length must be <= 255 for the handshake field.
    """
    tail_len = 1 + len(wire)  # the NUL separator plus the marker payload
    cases = {}

    def case(name, host):
        total = len(host) + tail_len
        cases[name] = {
            "hostname": host,
            "hostname_length": len(host),
            "appended_tail_length": tail_len,
            "total_length": total,
            "fits_protocol_limit_255": total <= PROTOCOL_HOST_MAX,
        }

    case("short", HOSTNAME)

    # Near-limit: largest hostname that still fits (total == 255).
    max_host_len = PROTOCOL_HOST_MAX - tail_len
    suffix = ".example.test"
    label = "long-" + "a" * (max_host_len - len(suffix) - len("long-"))
    near = label + suffix
    assert len(near) == max_host_len
    case("near_limit_fits", near)

    # One byte over the limit must be rejected by the calculator.
    over = near + "b"
    case("near_limit_exceeds", over)
    return {"appended_tail_length_formula": "1 + len(wire)", "cases": cases}


def main() -> int:
    out = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else (
        pathlib.Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "floodgate_handshake_v0.json"
    )

    plaintext = "\0".join(v for _, v in FIELDS)
    wire = encrypt_handshake_data(plaintext)
    host = raw_handshake_host(HOSTNAME, wire)

    xuid = FIELDS[2][1]
    fixture = {
        "_meta": {
            "synthetic": True,
            "description": "Synthetic, deterministic differential fixture. NOT captured from a live Geyser/Floodgate instance. Contains no real player data.",
            "generator": "tools/generate_floodgate_fixture.py",
            "framing_source": "GeyserMC/Geyser common/src/main/java/org/geysermc/floodgate/{util/BedrockData,crypto/AesCipher,crypto/FloodgateCipher,crypto/Base64Topping}.java (master, fetched 2026-10-26)",
            "primitive": "AES-128-GCM via python cryptography AESGCM (independent of the Java sources)",
        },
        "key": {
            "hex": KEY.hex(),
            "length_bytes": len(KEY),
            "encoding": "raw bytes (Geyser AesKeyProducer.produceFrom -> SecretKeySpec)",
            "test_only": True,
        },
        "iv": {"hex": IV.hex(), "length_bytes": len(IV), "tag_bits": 128, "aad": None},
        "framing": {
            "header": HEADER,
            "header_hex": HEADER.encode("ascii").hex(),
            "splitter": SPLITTER,
            "splitter_byte": 0x21,
            "iv_encoding": "base64 (standard, padded)",
            "payload_encoding": "base64 of ciphertext||16-byte-tag",
            "wire_format": "HEADER + b64(iv) + '!' + b64(ciphertext||tag)",
        },
        "hostname": HOSTNAME,
        "raw_handshake_host": host,
        "raw_handshake_host_repr": host.replace("\0", "\\0"),
        "plaintext_bedrock_data": plaintext,
        "plaintext_bedrock_data_repr": plaintext.replace("\0", "\\0"),
        "expectations": {
            "field_count": EXPECTED_LENGTH,
            "fields": {k: v for k, v in FIELDS},
            "linked_player_present": False,
            "from_proxy": False,
            "xuid": xuid,
            "uuid": str(uuid.UUID(int=int(xuid))),
            "uuid_derivation": "new UUID(0, xuid) (Floodgate Utils.getJavaUuid)",
            "raw_username": FIELDS[1][1],
            "expected_prefixed_username": expected_username(FIELDS[1][1]),
            "username_derivation": "prefix '.' + space->'_' per Floodgate config.yml defaults; call site not line-verified in this run",
        },
        "length_calculator": build_length_cases(wire),
        "negative_cases": {
            "wrong_key_hex": bytes(b ^ 0xFF for b in KEY).hex(),
            "missing_marker_host": HOSTNAME + "\0not-floodgate-data",
            "eleven_field_plaintext_note": "BedrockData.fromString rejects non-12 field counts (EXPECTED_LENGTH)",
        },
    }

    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(fixture, indent=2, ensure_ascii=True) + "\n", encoding="utf-8", newline="\n")
    print(f"wrote {out}")
    print(f"wire_length={len(wire)} raw_host_length={len(host)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
