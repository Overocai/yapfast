"""The key server's tests: a valid key, an invalid one, a revoked one, the
invites, and the admin API's secret. They start the server on free ports in
a folder of their own and talk to it over HTTP, as Yapfast and the admin
commands do: `python keys/test_yapfast_keys.py`."""

import json
import os
import sys
import tempfile
import unittest
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import yapfast_keys as keys  # noqa: E402 - the folder goes on the path first

SECRET = "yka_test-secret"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class KeyServer(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.public, self.admin, self.store = keys.start_servers(
            Path(self.folder.name) / "keys.sqlite3", keys.sha256_hex(SECRET), 0, 0)

    def tearDown(self):
        for server in (self.public, self.admin):
            server.shutdown()
            server.server_close()
        self.store.close()
        self.folder.cleanup()

    def call(self, server, method, path, body=None, secret=None):
        headers = {"Content-Type": "application/json"}
        if secret:
            headers["Authorization"] = f"Bearer {secret}"
        request = urllib.request.Request(
            f"http://127.0.0.1:{server.server_address[1]}{path}", method=method,
            data=None if body is None else json.dumps(body).encode(), headers=headers)
        try:
            with OPENER.open(request, timeout=10) as response:
                return response.status, json.loads(response.read())
        except urllib.error.HTTPError as error:
            return error.code, json.loads(error.read())

    def create(self, note="Niko", invites=1):
        status, made = self.call(self.admin, "POST", "/admin/keys",
                                 {"note": note, "invites": invites}, SECRET)
        self.assertEqual(status, 200)
        return made

    def validate(self, key):
        status, answer = self.call(self.public, "POST", "/v1/validate", {"key": key})
        self.assertEqual(status, 200)
        return answer

    def test_a_new_key_is_valid_and_kept_only_as_its_hash(self):
        made = self.create()
        self.assertRegex(made["key"], r"^YF(-[0-9A-HJKMNP-TV-Z]{5}){5}$")
        answer = self.validate(made["key"])
        self.assertEqual(answer["valid"], True)
        self.assertEqual(answer["invites"], {"quota": 1, "used": 0, "pending": 0, "left": 1})
        dump = "\n".join(self.store.db.iterdump())
        self.assertNotIn(made["key"], dump)
        self.assertNotIn(made["key"].replace("-", ""), dump)
        self.assertIn(keys.key_hash(made["key"]), dump)

    def test_a_key_typed_loosely_still_counts(self):
        key = self.create()["key"]
        loose = " " + key.lower().replace("-", " ") + " "
        self.assertTrue(self.validate(loose)["valid"])
        # O for zero and I or L for one, as Crockford's base32 reads them.
        if "0" in key or "1" in key:
            looked = key.replace("0", "o").replace("1", "l")
            self.assertTrue(self.validate(looked)["valid"])

    def test_an_invalid_key_is_refused(self):
        self.create()
        for wrong in [keys.new_key(), "YF-AAAAA", "not a key", ""]:
            self.assertEqual(self.validate(wrong), {"valid": False, "reason": "invalid"}, wrong)

    def test_a_revoked_key_is_refused_from_then_on(self):
        made = self.create()
        self.assertTrue(self.validate(made["key"])["valid"])
        status, answer = self.call(self.admin, "POST", "/admin/revoke",
                                   {"target": made["id"]}, SECRET)
        self.assertEqual((status, answer), (200, {"revoked": [made["id"]]}))
        self.assertEqual(self.validate(made["key"]), {"valid": False, "reason": "revoked"})
        # Revoking by the key itself finds it, and says nothing changed.
        status, answer = self.call(self.admin, "POST", "/admin/revoke",
                                   {"target": made["key"]}, SECRET)
        self.assertEqual((status, answer), (200, {"revoked": []}))

    def test_a_key_makes_one_invite_which_is_a_key_of_its_own(self):
        parent = self.create(invites=1)["key"]
        status, first = self.call(self.public, "POST", "/v1/invite", {"key": parent})
        self.assertEqual(status, 200)
        self.assertEqual(first["invites"], {"quota": 1, "used": 0, "pending": 1, "left": 0})
        # Unused, a new code replaces it.
        status, second = self.call(self.public, "POST", "/v1/invite", {"key": parent})
        self.assertEqual(status, 200)
        self.assertNotEqual(first["code"], second["code"])
        self.assertEqual(self.validate(first["code"]), {"valid": False, "reason": "revoked"})
        # Used, the invite is spent, and the invited key invites no one.
        invited = self.validate(second["code"])
        self.assertTrue(invited["valid"])
        self.assertEqual(invited["invites"]["quota"], 0)
        self.assertEqual(self.validate(parent)["invites"],
                         {"quota": 1, "used": 1, "pending": 0, "left": 0})
        status, refused = self.call(self.public, "POST", "/v1/invite", {"key": parent})
        self.assertEqual((status, refused["reason"]), (409, "no_invites"))
        status, refused = self.call(self.public, "POST", "/v1/invite", {"key": second["code"]})
        self.assertEqual((status, refused["reason"]), (409, "no_invites"))

    def test_the_admin_may_give_an_invited_key_invites_and_revoke_down_the_line(self):
        made = self.create(invites=1)
        _, invite = self.call(self.public, "POST", "/v1/invite", {"key": made["key"]})
        self.validate(invite["code"])
        status, changed = self.call(self.admin, "POST", "/admin/invites",
                                    {"target": invite["code"], "quota": 1}, SECRET)
        self.assertEqual((status, changed["invites"]["left"]), (200, 1))
        status, answer = self.call(self.admin, "POST", "/admin/revoke",
                                   {"target": made["id"], "cascade": True}, SECRET)
        self.assertEqual(status, 200)
        self.assertEqual(len(answer["revoked"]), 2)
        self.assertEqual(self.validate(invite["code"])["reason"], "revoked")

    def test_a_revoked_or_invalid_key_makes_no_invite(self):
        made = self.create()
        self.call(self.admin, "POST", "/admin/revoke", {"target": made["id"]}, SECRET)
        for key, reason in [(made["key"], "revoked"), (keys.new_key(), "invalid")]:
            status, refused = self.call(self.public, "POST", "/v1/invite", {"key": key})
            self.assertEqual((status, refused["reason"]), (403, reason))

    def test_the_admin_api_takes_only_the_secret(self):
        for secret in [None, "wrong", SECRET + "x"]:
            status, _ = self.call(self.admin, "POST", "/admin/keys", {"note": "x"}, secret)
            self.assertEqual(status, 401, secret)
            status, _ = self.call(self.admin, "GET", "/admin/keys", None, secret)
            self.assertEqual(status, 401, secret)
        self.assertEqual(self.call(self.admin, "GET", "/admin/keys", None, SECRET)[0], 200)

    def test_the_public_api_has_no_admin_side(self):
        for path in ["/admin/keys", "/admin/revoke", "/admin/invites"]:
            status, _ = self.call(self.public, "POST", path, {"note": "x"}, SECRET)
            self.assertEqual(status, 404, path)

    def test_the_listing_names_no_key(self):
        made = self.create()
        status, listing = self.call(self.admin, "GET", "/admin/keys", None, SECRET)
        self.assertEqual(status, 200)
        self.assertNotIn(made["key"], json.dumps(listing))
        self.assertEqual(listing["keys"][0]["id"], made["id"])

    def test_bad_requests_are_refused(self):
        for body in [None, {"nokey": 1}, {"key": 5}]:
            status, _ = self.call(self.public, "POST", "/v1/validate", body)
            self.assertEqual(status, 400, body)
        status, _ = self.call(self.public, "POST", "/v1/validate", {"key": "x" * 5000})
        self.assertEqual(status, 400)
        self.assertEqual(self.call(self.public, "GET", "/v1/health")[0], 200)


if __name__ == "__main__":
    unittest.main(verbosity=2)
