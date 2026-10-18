"""Verify both supported Futu signing algorithms without stored test secrets."""
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import urllib.error
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("futu_readonly", ROOT / "integrations/futu-readonly/server.py")
futu = importlib.util.module_from_spec(spec)
spec.loader.exec_module(futu)
NODE = os.environ.get("FUTU_LEGACY_NODE") or shutil.which("node")


@unittest.skipUnless(NODE, "Node.js is required for request signatures")
class FutuReadOnlyTests(unittest.TestCase):
    def key(self, root, algorithm):
        path = Path(root) / "private_key.pem"
        subprocess.run([NODE, "--input-type=module", "-e", """
import {generateKeyPairSync} from 'node:crypto';
import {writeFileSync} from 'node:fs';
const type=process.argv[2];
const pair=generateKeyPairSync(type,type==='rsa'?{modulusLength:2048}:type==='ec'?{namedCurve:'prime256v1'}:{});
writeFileSync(process.argv[1],pair.privateKey.export({type:'pkcs8',format:'pem'}),{mode:0o600});
writeFileSync(process.argv[1]+'.public',pair.publicKey.export({type:'spki',format:'pem'}));
""", str(path), algorithm], check=True, capture_output=True)
        return path

    def verify_signature(self, key, payload, signature, algorithm):
        result = subprocess.run([NODE, "--input-type=module", "-e", """
import {readFileSync} from 'node:fs';
import {verify,constants} from 'node:crypto';
const input=JSON.parse(readFileSync(0,'utf8'));
const key=readFileSync(process.argv[1]+'.public');
const rsa=process.argv[2]==='rsa';
console.log(verify(rsa?'sha256':null,Buffer.from(input.payload),rsa?{key,padding:constants.RSA_PKCS1_PADDING}:key,Buffer.from(input.signature,'base64')));
""", str(key), algorithm], input=json.dumps({"payload": payload, "signature": signature}), text=True, capture_output=True, check=True)
        return result.stdout.strip() == "true"

    def test_ed25519_signs_original_request_bytes(self):
        with tempfile.TemporaryDirectory() as root:
            key = self.key(root, "ed25519")
            payload = "1700000000000\nGET\n/api/v1.0/accounts/authorized_trd_accs\n\n"
            with patch.dict(os.environ, {"FUTU_LEGACY_NODE": NODE}):
                signature = futu.sign_payload(key, payload)
            self.assertTrue(self.verify_signature(key, payload, signature, "ed25519"))
            self.assertFalse(self.verify_signature(key, payload.rstrip(), signature, "ed25519"))

    def test_rsa_keeps_sha256_pkcs1_compatibility(self):
        with tempfile.TemporaryDirectory() as root:
            key = self.key(root, "rsa")
            payload = "1700000000000\nGET\n/api/v1.0/quote/trading-days\nmarket=HK\n"
            with patch.dict(os.environ, {"FUTU_LEGACY_NODE": NODE}):
                signature = futu.sign_payload(key, payload)
            self.assertTrue(self.verify_signature(key, payload, signature, "rsa"))

    def test_unsupported_algorithm_and_unsafe_key_fail_without_key_output(self):
        with tempfile.TemporaryDirectory() as root:
            key = self.key(root, "ec")
            with patch.dict(os.environ, {"FUTU_LEGACY_NODE": NODE}):
                with self.assertRaisesRegex(futu.FutuError, "signature could not be created"):
                    futu.sign_payload(key, "request")
                key.chmod(0o644)
                with self.assertRaises(futu.FutuError):
                    futu.sign_payload(key, "request")

    def test_credentials_reject_symlinks_and_shared_permissions(self):
        with tempfile.TemporaryDirectory() as root:
            key = self.key(root, "ed25519")
            appkey = Path(root) / "appkey_id"
            appkey.write_text("fixture-appkey")
            appkey.chmod(0o600)
            with patch.dict(os.environ, {"FUTU_LEGACY_CREDENTIAL_DIR": root}):
                self.assertEqual(futu.credential_paths(), (appkey, key))
                appkey.chmod(0o644)
                with self.assertRaises(futu.FutuError):
                    futu.credential_paths()
                appkey.chmod(0o600)
                saved = Path(root) / "saved.pem"
                key.rename(saved)
                key.symlink_to(saved)
                with self.assertRaises(futu.FutuError):
                    futu.credential_paths()

    def test_api_rejection_does_not_echo_response_secrets(self):
        with tempfile.TemporaryDirectory() as root:
            key = self.key(root, "ed25519")
            appkey = Path(root) / "appkey_id"
            appkey.write_text("fixture-appkey")
            appkey.chmod(0o600)
            rejected = urllib.error.HTTPError("https://example.invalid", 403, "Forbidden", {}, io.BytesIO(b'{"code":-12009,"message":"sensitive-provider-detail"}'))
            with patch.dict(os.environ, {"FUTU_LEGACY_CREDENTIAL_DIR": root, "FUTU_LEGACY_NODE": NODE}), patch.object(futu.urllib.request, "urlopen", side_effect=rejected):
                with self.assertRaises(futu.FutuError) as result:
                    futu.invoke_tool("futu_account_authorized_trd_accs", {})
            self.assertIn("HTTP 403; API code -12009", str(result.exception))
            self.assertNotIn("sensitive-provider-detail", str(result.exception))
            self.assertEqual(len(futu.tool_specs()), 3)
            with self.assertRaisesRegex(futu.FutuError, "Unknown read-only"):
                futu.invoke_tool("place_order", {"account_id": "1234"})


if __name__ == "__main__":
    unittest.main()
