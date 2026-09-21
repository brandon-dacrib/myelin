/**
 * A password for an account an administrator is creating for somebody else.
 *
 * Twenty characters from an alphabet with the look-alikes taken out (no `0`/`O`, `1`/`l`/`I`),
 * because this one gets read off a screen, pasted into a chat, or typed on a phone by the
 * person it is for. 20 of 57 symbols is about 116 bits, from `crypto.getRandomValues`, with
 * rejection sampling so that every symbol is equally likely rather than the first few being
 * favoured by a modulo.
 */
const ALPHABET = "abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

export function generatePassword(length = 20): string {
  const limit = 256 - (256 % ALPHABET.length);
  let out = "";
  while (out.length < length) {
    const bytes = crypto.getRandomValues(new Uint8Array(length * 2));
    for (const byte of bytes) {
      if (byte < limit && out.length < length) out += ALPHABET[byte % ALPHABET.length];
    }
  }
  return out;
}
