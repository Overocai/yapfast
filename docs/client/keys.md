# The key gate

A build made with `YAPFAST_KEYS_URL` set (the workflow takes it from the
`YAPFAST_KEYS_URL` Actions variable, or from the key server a run started by
hand names) opens only with a valid key, which the key server in `keys/`
makes and checks. A build without it opens as ever. `keygate.rs` holds all
of it.

- Before anything else shows, the window asks for the key, on a card on the
  rail's ground as the sign-in screen's: the field, Unlock (Enter too), and
  why the last key typed didn't open it. A valid key is kept in the OS
  credential store (`io.github.yapfast.Yapfast`, `license-key`, beside the
  Discord token), so it's typed once.
- The saved session, or the preview, starts only once a key was valid: until
  then nothing connects to Discord. The kept key is checked as Yapfast opens
  and every minute after (`POST /v1/validate`). A check that can't reach the
  server is tried once more five seconds later before the window locks, as
  the network may come up just after Windows does.
- An invalid or revoked key, or a server that can't be reached, locks the
  window: Yapfast is locked, why in the danger's ink, Try again and Use
  another key; it also tries again every minute. A window that locks while
  open leaves the call, so no one stays in it with no way to mute, and shows
  and plays no alerts and takes no shortcuts until it opens again; what it
  showed comes back as it was. Started in the tray, the window opens to ask
  for a key, or to say why it's locked.
- My Account shows the key with all but its last group hidden and when it
  was last found valid, and, as the server counts them, the invites it may
  make: Generate invite code asks the server for a code (`POST /v1/invite`),
  which is a key of its own for one person, shown with Copy. While that code
  is unused, a new one replaces it; once used, the invite is spent.
- The key server is HTTPS, or for a test build this computer's own address
  over HTTP. Its client follows no redirect and gives up after 15 seconds.

There's no DRM, hardware ID, obfuscation or anti-tamper: the key and the
server's answer are all there is, and a build is shipped with its source.
