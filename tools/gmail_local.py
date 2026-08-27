#!/usr/bin/env python3
"""gmail_local - READ-ONLY Gmail access for the local AI, via IMAP + a Gmail app password.

Lets the local agent do what Claude did: search Jep's inbox (orders, receipts, anything) and read a
message, grounding answers in his real email. READ-ONLY by design - it can search and read, it can
NEVER send/reply/delete (matches Jep's standing "never send emails" rule).

Setup (one time): turn on 2-Step Verification on the Google account, create an App Password
(myaccount.google.com -> Security -> App passwords), then:  setx GMAIL_APP_PASSWORD "<16-char pw>"
The password lives only in Jep's environment, never in source.

Uses Gmail's X-GM-RAW so full Gmail search syntax works (from:, subject:, newer_than:, etc).

    python tools/gmail_local.py search "from:currys" [n]
    python tools/gmail_local.py read <uid>
"""
from __future__ import annotations

import email
import imaplib
import os
import re
import sys
from email.header import decode_header

IMAP_HOST = "imap.gmail.com"
GMAIL_USER = os.environ.get("GMAIL_USER", "jepeshan@gmail.com")
GMAIL_PW = os.environ.get("GMAIL_APP_PASSWORD", "")


def _connect():
    if not GMAIL_PW:
        raise RuntimeError("GMAIL_APP_PASSWORD is not set - create a Gmail app password and "
                           "`setx GMAIL_APP_PASSWORD \"...\"`, then reopen the terminal.")
    m = imaplib.IMAP4_SSL(IMAP_HOST)
    m.login(GMAIL_USER, GMAIL_PW)
    return m


def _decode(s):
    if not s:
        return ""
    out = []
    for part, enc in decode_header(s):
        if isinstance(part, bytes):
            out.append(part.decode(enc or "utf-8", "replace"))
        else:
            out.append(part)
    return "".join(out)


def _body_text(msg) -> str:
    """Pull readable text from a message (prefer text/plain; strip tags from html)."""
    if msg.is_multipart():
        for part in msg.walk():
            if part.get_content_type() == "text/plain" and "attachment" not in str(part.get("Content-Disposition")):
                try:
                    return part.get_payload(decode=True).decode(part.get_content_charset() or "utf-8", "replace")
                except Exception:
                    pass
        for part in msg.walk():
            if part.get_content_type() == "text/html":
                try:
                    html = part.get_payload(decode=True).decode(part.get_content_charset() or "utf-8", "replace")
                    return re.sub(r"\s+", " ", re.sub(r"(?s)<[^>]+>", " ", html)).strip()
                except Exception:
                    pass
        return ""
    try:
        payload = msg.get_payload(decode=True).decode(msg.get_content_charset() or "utf-8", "replace")
    except Exception:
        payload = str(msg.get_payload())
    if msg.get_content_type() == "text/html":
        payload = re.sub(r"\s+", " ", re.sub(r"(?s)<[^>]+>", " ", payload))
    return payload.strip()


def search(query: str, n: int = 10):
    """Gmail-style search (X-GM-RAW). Returns [{uid, date, sender, subject, snippet}] newest first."""
    m = _connect()
    try:
        m.select("INBOX", readonly=True)
        typ, data = m.uid("search", None, "X-GM-RAW", '"%s"' % query.replace('"', ""))
        uids = data[0].split()[-n:][::-1] if data and data[0] else []
        out = []
        for uid in uids:
            typ, d = m.uid("fetch", uid, "(BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)])")
            if typ != "OK" or not d or not d[0]:
                continue
            hdr = email.message_from_bytes(d[0][1])
            out.append({"uid": uid.decode(), "date": _decode(hdr.get("Date", "")),
                        "sender": _decode(hdr.get("From", "")), "subject": _decode(hdr.get("Subject", ""))})
        return out
    finally:
        try:
            m.logout()
        except Exception:
            pass


def read(uid: str, cap: int = 6000) -> dict:
    m = _connect()
    try:
        m.select("INBOX", readonly=True)
        typ, d = m.uid("fetch", uid.encode(), "(RFC822)")
        if typ != "OK" or not d or not d[0]:
            return {"error": "message %s not found" % uid}
        msg = email.message_from_bytes(d[0][1])
        body = _body_text(msg)
        return {"uid": uid, "date": _decode(msg.get("Date", "")), "sender": _decode(msg.get("From", "")),
                "subject": _decode(msg.get("Subject", "")), "body": body[:cap]}
    finally:
        try:
            m.logout()
        except Exception:
            pass


def main():
    if len(sys.argv) < 3:
        print("usage: gmail_local.py search \"<gmail query>\" [n]  |  read <uid>"); return 2
    cmd = sys.argv[1]
    if cmd == "search":
        n = int(sys.argv[3]) if len(sys.argv) > 3 and sys.argv[3].isdigit() else 10
        for r in search(sys.argv[2], n):
            print("[%s] %s | %s | %s" % (r["uid"], r["date"][:22], r["sender"][:34], r["subject"][:60]))
    elif cmd == "read":
        r = read(sys.argv[2])
        if r.get("error"):
            print(r["error"]); return 1
        print("From: %s\nDate: %s\nSubject: %s\n%s\n%s" % (r["sender"], r["date"], r["subject"], "-" * 50, r["body"]))
    else:
        print("unknown command:", cmd); return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
