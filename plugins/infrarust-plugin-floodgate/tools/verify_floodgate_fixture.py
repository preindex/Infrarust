#!/usr/bin/env python3
"""Independently verify the synthetic Floodgate fixture.

This does NOT import the generator. It re-implements the parse path of the
Geyser sources and decrypts with the low-level hazmat `Cipher` API:

  * FloodgateCipher.version()/HEADER: marker "^Floodgate^" + char(0x3E + version)
  * Floodgate separateHostname(): split raw host on NUL, pick marker segment
  * AesCipher.decrypt(): find '!' (0x21) splitter, base64-decode iv and payload,
    split tag (last 16 bytes, JCE appends it), AES-GCM decrypt, no AAD
  * BedrockData.fromString(): Java-style split on NUL (trailing empties dropped),
    must be 12 fields; "null" => no link, "1"/"0" => fromProxy

Usage:
  python -I tools/verify_floodgate_fixture.py [FIXTURE_JSON]
"""

import base64
import json
import pathlib
import sys
import uuid

from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

HEADER_ID = "^Floodgate^"
EXPECTED_LENGTH = 12
TAG_LEN = 16
SPLITTER = "!"


def marker_version(segment: str) -> int:
    # Mirrors FloodgateCipher.version(): returns -1 when not a marker segment
    if len(segment) <= len(HEADER_ID) + 1:
        return -1
    if not segment.startswith(HEADER_ID):
        return -1
    return ord(segment[len(HEADER_ID)]) - 0x3E


def separate_hostname(raw: str):
    """Returns (data_segment or None, remaining hostname)."""
    data = None
    rest = []
    for seg in raw.split("\0"):
        if data is None and marker_version(seg) != -1:
            data = seg
            continue
        rest.append(seg)
    return data, "\0".join(rest)


def decrypt_wire(wire: str, key: bytes) -> str:
    # Mirrors AesCipher.decrypt() with Base64Topping
    if not wire.startswith(HEADER_ID):
        raise ValueError("missing marker")
    body = wire[len(HEADER_ID) + 1:]  # skip the version char
    iv_b64, sep, payload_b64 = body.partition(SPLITTER)
    if not sep:
        raise ValueError("missing splitter")
    iv = base64.b64decode(iv_b64, validate=True)
    sealed = base64.b64decode(payload_b64, validate=True)
    if len(iv) != 12:
        raise ValueError(f"iv length {len(iv)} != 12")
    ciphertext, tag = sealed[:-TAG_LEN], sealed[-TAG_LEN:]
    dec = Cipher(algorithms.AES(key), modes.GCM(iv, tag)).decryptor()  # no AAD in Floodgate
    return (dec.update(ciphertext) + dec.finalize()).decode("utf-8")


def bedrock_from_string(data: str):
    parts = data.split("\0")
    while parts and parts[-1] == "":  # Java String.split drops trailing empties
        parts.pop()
    if len(parts) != EXPECTED_LENGTH:
        return None, len(parts)
    p = parts
    fields = {
        "version": p[0], "username": p[1], "xuid": p[2], "deviceOs": int(p[3]),
        "languageCode": p[4], "uiProfile": int(p[5]), "inputMode": int(p[6]),
        "ip": p[7], "linkedPlayer": None if p[8] == "null" else p[8],
        "fromProxy": p[9] == "1", "subscribeId": int(p[10]), "verifyCode": p[11],
    }
    return fields, len(parts)


def prefixed(raw_username: str) -> str:
    return "." + raw_username.replace(" ", "_")


def main() -> int:
    path = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else (
        pathlib.Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "floodgate_handshake_v0.json"
    )
    fx = json.loads(path.read_text(encoding="utf-8"))
    key = bytes.fromhex(fx["key"]["hex"])
    exp = fx["expectations"]
    failures = []

    def check(name, cond):
        print(("PASS " if cond else "FAIL ") + name)
        if not cond:
            failures.append(name)

    # 1. host separation finds the marker segment
    data, host = separate_hostname(fx["raw_handshake_host"])
    check("marker segment located", data is not None)
    check("hostname restored", host == fx["hostname"])
    check("marker is ^Floodgate^>", data is not None and data[:12] == "^Floodgate^>")

    # 2. decrypt and compare plaintext
    plain = decrypt_wire(data, key)
    check("decrypted plaintext equals fixture plaintext", plain == fx["plaintext_bedrock_data"])

    # 3. BedrockData field parse
    fields, n = bedrock_from_string(plain)
    check(f"field count == {EXPECTED_LENGTH}", n == EXPECTED_LENGTH and fields is not None)
    if fields:
        for k, v in exp["fields"].items():
            got = fields[k]
            if k == "linkedPlayer":
                got = "null" if got is None else got
            if k == "fromProxy":
                got = "1" if got else "0"
            check(f"field {k} == {v!r}", str(got) == str(v))
        check("unlinked => linkedPlayer is None", fields["linkedPlayer"] is None)
        check("fromProxy false", fields["fromProxy"] is False)
        check("uuid == new UUID(0, xuid)", str(uuid.UUID(int=int(fields["xuid"]))) == exp["uuid"])
        check("prefixed username", prefixed(fields["username"]) == exp["expected_prefixed_username"])
        check("prefixed username is .Bedrock_User", exp["expected_prefixed_username"] == ".Bedrock_User")

    # 4. negative cases must fail
    try:
        decrypt_wire(data, bytes.fromhex(fx["negative_cases"]["wrong_key_hex"]))
        check("wrong key rejected", False)
    except InvalidTag:
        check("wrong key rejected", True)

    tampered = data[:-3] + ("A" if data[-3] != "A" else "B") + data[-2:]
    try:
        decrypt_wire(tampered, key)
        check("tampered ciphertext rejected", False)
    except (InvalidTag, ValueError):
        check("tampered ciphertext rejected", True)

    check("non-floodgate segment not a marker",
          separate_hostname(fx["negative_cases"]["missing_marker_host"])[0] is None)

    # 5. length calculator
    lc = fx["length_calculator"]["cases"]
    wire_len = len(data)
    for name, case in lc.items():
        expected_total = case["hostname_length"] + 1 + wire_len
        check(f"length[{name}] total == len(host)+1+len(wire)", case["total_length"] == expected_total)
        check(f"length[{name}] fits-255 flag consistent", case["fits_protocol_limit_255"] == (expected_total <= 255))
    check("near_limit_fits is exactly 255", lc["near_limit_fits"]["total_length"] == 255)
    check("near_limit_exceeds is 256", lc["near_limit_exceeds"]["total_length"] == 256)

    print(f"\n{'OK' if not failures else 'FAILED'}: {len(failures)} failing check(s)")
    return 0 if not failures else 1


if __name__ == "__main__":
    raise SystemExit(main())
