import { readFileSync, lstatSync } from 'node:fs';
import { createPrivateKey, sign, constants } from 'node:crypto';

// Key material stays in a private host file; only the request signature leaves
// this helper. Ed25519 signs the original bytes without a separate digest.
try {
  const path = process.argv[2];
  const info = lstatSync(path);
  if (!info.isFile() || (info.mode & 0o077) || (process.geteuid && info.uid !== process.geteuid())) throw new Error('Unsafe key file');
  const key = createPrivateKey(readFileSync(path));
  const payload = readFileSync(0);
  let signature;
  if (key.asymmetricKeyType === 'ed25519') signature = sign(null, payload, key);
  else if (key.asymmetricKeyType === 'rsa') signature = sign('sha256', payload, { key, padding: constants.RSA_PKCS1_PADDING });
  else throw new Error('Unsupported signing algorithm');
  process.stdout.write(signature.toString('base64'));
} catch {
  process.stderr.write('Futu AppKey signing failed.\n');
  process.exitCode = 1;
}
