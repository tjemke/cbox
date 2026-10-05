# cbox implementation notes

Things a later agent would otherwise have to rediscover. Written while debugging
`Unable to connect to API (ENOTIMP)` inside a box started by a bare `cbox`.

Source references to passt are against the upstream tree as of
`912ad4f8bc69e143d6ad571e5e2e88206444d8c7` (2026-09-27). Line numbers drift;
the function names won't.

---

## 1. `Unable to connect to API (ENOTIMP)` — guest DNS went to the host's loopback

**Symptom.** The box boots, the OAuth token is in the environment, Claude Code's
TUI shows the login as valid, and every request fails with
`Unable to connect to API (ENOTIMP)`.

The login looking fine is a red herring: `CLAUDE_CODE_OAUTH_TOKEN` is read out of
the environment and rendered locally. Nothing is contacted to draw that screen.

### Where `ENOTIMP` comes from

`nixpkgs.claude-code` installs the **Bun-compiled native binary**, not a Node
script — the derivation unzstds a prebuilt binary and notes "otherwise the bun
runtime is executed instead of the binary". That matters, because:

- Bun's default DNS backend **on Linux is c-ares**, not `getaddrinfo`. c-ares
  parses `/etc/resolv.conf` itself and bypasses NSS entirely.
- c-ares maps DNS `RCODE 4 (NotImp)` to `ARES_ENOTIMP`
  (`src/lib/record/ares_dns_mapping.c`, `ares_process.c`), and Bun surfaces that
  verbatim as the string `ENOTIMP`.
- Claude Code's error formatter has no case for `ENOTIMP`, so it falls through to
  the default arm: `` `Unable to connect to API (${n})` ``.

So `ENOTIMP` is not a connectivity error at all. It means **a DNS server
answered the guest, and answered "not implemented"**. Something was listening;
it just wasn't a resolver that would answer the question.

### The actual breakage

`cbox.sh` used to start passt with both of these:

```
--dns-forward 169.254.1.1 --dns 169.254.1.1
```

They look like a matched pair. They are not. passt has **three** separate DNS
knobs (`struct ip4_ctx` in `passt.h`):

| field | set by | meaning |
|---|---|---|
| `dns[]` | `-D` / `--dns` | addresses **advertised to the guest** over DHCP |
| `dns_match` | `--dns-forward` | address the guest sends to, which passt intercepts |
| `dns_host` | `--dns-host` | resolver on the host that intercepted queries are **forwarded to** |

`--dns-host` is the one that matters and it was never set. Its documented
default is "the first nameserver from the host's `/etc/resolv.conf`" — and
`--dns` is what stops that default from ever being applied:

```c
/* conf.c:216 */ dns4_set = !c->ifi4 || !IN4_IS_ADDR_UNSPECIFIED(&c->ip4.dns[0]);
/* conf.c:219 */ dns_set  = (dns4_set && dns6_set) || c->no_dns;
...
/* conf.c:229 */ if (!dns_set && strstr(line, "nameserver ") == line) {
/* conf.c:238 */     add_dns_resolv(c, p + 1, dns4_set ? NULL : &dns4_idx, ...);
```

`-D 169.254.1.1` writes `ip4.dns[0]`, so `dns4_set` is true. `--ipv4-only`
leaves `ifi6 == 0`, so `dns6_set` is true too. `dns_set` is therefore true and
**every `nameserver` line in the host's `/etc/resolv.conf` is skipped**.
`add_dns_resolv4()` — the only code that assigns `ip4.dns_host`
(`conf.c:117`) — is never reached.

Note this is not specific to `--ipv4-only`: `idx4` is passed as `NULL` whenever
`dns4_set` is true (`conf.c:195`), so **any IPv4 `-D` argument suppresses IPv4
`dns_host` autodetection**, on its own.

`ip4.dns_host` therefore stays `0.0.0.0`, and the forwarding path uses it
unconditionally:

```c
/* fwd.c:909 */ if (is_dns_flow(proto, ini) &&
                    inany_equals4(&ini->oaddr, &c->ip4.dns_match))
/* fwd.c:911 */         tgt->eaddr = inany_from_v4(c->ip4.dns_host);   /* 0.0.0.0 */
```

passt then `connect()`s a host-side socket to `0.0.0.0:53`. On Linux,
**connecting to `0.0.0.0` is connecting to `127.0.0.1`** — verified here:

```
UDP connect(0.0.0.0:5399) peer = {"address":"127.0.0.1",...}
TCP connect(0.0.0.0:5399) -> remote 127.0.0.1   (server bound only to 127.0.0.1)
```

So every DNS query from the box was being delivered to **the cbox host's own
`127.0.0.1:53`**, regardless of what the host's real resolver is. What you get
back depends entirely on what happens to be sitting on that port on that
machine:

- nothing listening → ICMP port unreachable → `ECONNREFUSED`
- systemd-resolved (which binds `127.0.0.53`, *not* `127.0.0.1`) → also nothing
- some other local listener that isn't a recursive resolver → a bogus rcode;
  `NOTIMP` / `REFUSED` / `SERVFAIL`

The reported `ENOTIMP` is the third case. The identity of that listener is
host-local and not something this repo can know; it's also not worth chasing,
because the destination was wrong in the first place.

### Why nothing warned

Three independent reasons, all worth remembering:

1. `get_dns()` only warns `"Couldn't get any nameserver address"` **inside**
   `if (!dns_set)`. With `-D` given, `dns_set` is true, so the warning is
   unreachable in exactly the case that breaks.
2. `conf_print()` prints `ip4.dns[]` and the search list. It **never prints
   `dns_host`**. So even a non-quiet passt would have logged a cheerful
   `DNS: 169.254.1.1` and looked completely healthy.
3. `cbox.sh` passed `--quiet`, which drops `conf_print()` entirely.

The only place the real target is visible is `flow_log__()` at debug level, which
prints both flowsides when a flow reaches `FLOW_STATE_TGT`:

```
Flow 0 (UDP): INI -> TGT
    TAP [<guest>]:NNN -> [169.254.1.1]:53 => HOST [0.0.0.0]:NNN -> [0.0.0.0]:53
                                                                   ^^^^^^^^^ smoking gun
```

### The fix

Drop `--dns`. It was redundant to begin with — the guest does not take its
resolver from DHCP (`dhcpV4Config.UseDNS = false`; `networking.nameservers`
pins `169.254.1.1` statically), so advertising it over DHCP bought nothing while
costing the `--dns-host` default.

```sh
passt --foreground --one-off --ipv4-only --socket "$RUN/passt.sock" \
  --no-map-gw --dns-forward 169.254.1.1 --search none
```

With `-D` gone, `get_dns()` parses the host's `resolv.conf` and
`add_dns_resolv4()` sets `dns_host` to the first nameserver — **including a
loopback one**, because `dns_host` is assigned (`conf.c:117`) *before* the
loopback special-casing. Both host shapes then work:

- host resolver on loopback (`127.0.0.53`, systemd-resolved): `dns_host =
  127.0.0.53`. Because `dns_match` is already set and `map_host_loopback` is
  unspecified under `--no-map-gw`, `add_dns_resolv4()` takes its `else` branch
  and advertises `dns_match` over DHCP instead. That branch's own comment names
  this exact configuration: *"No general host mapping, but requested for DNS
  (--dns-forward and --no-map-gw)"*. cbox is a supported case; `--dns` was the
  only thing taking it off the rails.
- host resolver on the LAN (`192.168.x.1`): `dns_host` is that address, the
  guest keeps querying `169.254.1.1`, passt NATs it host-side.

`--dns-forward` + `--no-map-gw` is **not** in conflict, despite how the man page
reads. The loopback-mapping teardown under `--no-map-gw` applies to
`nat_outbound()`/`nat_inbound()`; the DNS branch in `fwd_nat_from_tap()` runs
*before* `nat_outbound()` and is independent of it, and replies come back
through the stored flowside rather than `nat_inbound()`. Verified by reading
`fwd.c`, not inferred from the man page.

Also changed, deliberately:

- **`--search none`** — stops the host's internal DNS search domains being
  handed to the box. `get_dns()` still parses nameserver lines, because
  `--search none` only sets `dnss_set`, and the early return needs
  `dns_set && dnss_set`. Safe *only* as long as `-D` stays gone; with both, the
  function returns before opening `resolv.conf` at all and you are back to this
  bug.
- **dropped `--quiet`** — `passt.log` is already a file, info-level logging in
  passt is startup/teardown only (no per-flow spam; per-flow logging is
  `--debug`/`--trace`), so the startup config dump is free and is the only
  record of what the box's network looked like.

### Reproduced, with no VM at all

This was later confirmed against a real passt, without KVM, vhost-vsock, a tap
device or root — see `test/passt-dns/probe.sh`. passt's `--socket` mode speaks
qemu's `-netdev stream` framing (4-byte big-endian length, then one raw Ethernet
frame) and does not care that the thing on the other end is a 100-line script
rather than a virtual machine. So the whole DNS path can be exercised directly:
send one real query to `169.254.1.1:53` and read passt's `--debug` flow log.

With the old flags:

```
TAP [192.0.2.2]:5555 -> [169.254.1.1]:53 => HOST [0.0.0.0]:5555 -> [0.0.0.0]:53
TAP [192.0.2.2]:5555 -> [169.254.1.1]:53 => HOST [127.0.0.1]:5555 -> [0.0.0.0]:53
```

...and no reply. The second line is the proof: that source address is read back
off the socket *after* `connect()`, and the kernel picked `127.0.0.1` because
the destination `0.0.0.0` routes to loopback. With the current flags the same
query goes to `[10.0.2.3]:53` — the probe host's first `resolv.conf` nameserver
— and comes back `NOERROR` with an answer.

`./probe.sh repro` then closes the loop: in a throwaway network namespace (the
only way to bind `:53` unprivileged), with a server on `127.0.0.1:53` that
answers `RCODE 4` standing in for whatever is on the real host's loopback, the
guest receives `rcode=4 (NOTIMP)` — apparently *from 169.254.1.1*, since passt
rewrites the reply's source back. That is exactly the shape of the reported
failure.

The last link was checked separately: pointing a resolver at that same NotImp
server and calling node's `dns.resolve4()`, which uses c-ares like Bun does,
yields `code=ENOTIMP`. `dns.lookup()` — glibc `getaddrinfo` — yields `EAI_AGAIN`
for the identical response, which is the divergence described in §3.

### Verifying it, on a machine that can actually boot a box

```sh
# What the guest was hitting before the fix — reproduce the symptom on the host:
dig @127.0.0.1 api.anthropic.com | grep -i status      # expect NOTIMP / refused / timeout

# Inside a box, after the fix:
getent hosts api.anthropic.com
curl -sS -o /dev/null -w '%{http_code}\n' https://api.anthropic.com/v1/messages
```

`curl` is the better first probe than `claude`: the breakage is in the resolver
path, so **plain `curl` fails too**. If `curl` resolves and `claude` doesn't,
it's a different bug. (The error strings differ because glibc's `getaddrinfo`
collapses rcodes into `EAI_*`, while Bun's c-ares hands the raw rcode through —
same failure, different vocabulary.)

To see the NAT decision directly, add `--debug` to the passt line and run with
`cbox --keep`, then grep `passt.log` for `=> HOST`.

---

## 2. passt's DNS model, as a thing to keep in your head

Four rules that cover most of the footguns:

1. **`-D`/`--dns` is advertisement only.** It changes what the guest is *told*
   over DHCP. It never changes where passt forwards. Setting it suppresses
   `--dns-host` autodetection as a side effect.
2. **`--dns-forward` is a matcher**, not a destination. Upstream renamed the
   field `dns_fwd` → `dns_match` precisely to stop people reading it as a
   destination. It rejects unspecified, broadcast and loopback addresses
   (`conf.c`, option case 9), which is why `169.254.1.1` works and `127.0.0.1`
   would not.
3. **`--dns-host` is the destination**, defaulting to the host's first
   `resolv.conf` nameserver. Nothing validates that it ended up set, and
   `0.0.0.0` is a *silently working* destination because of the
   connect-to-INADDR_ANY rule.
4. **`-D none` is not "don't advertise".** It zeroes `dns_match` and `dns_host`
   as well (`conf.c`, case `'D'`), so it disables forwarding entirely, and it
   does so regardless of whether `--dns-forward` came earlier on the command
   line. Use `--no-dhcp-dns` if the intent is only to stop advertising.

---

## 3. Caveats found while reading the rest of the tree

### Security

- **The guest's nftables egress filter does not cover DNS, by construction.**
  `cbox-egress` permits `169.254.1.1:53/853` and rejects RFC1918 ranges, but the
  translation to the host's real resolver happens **in passt, on the host**,
  after the guest's filter has already passed the packet. If the host's resolver
  is a LAN or corporate resolver, the box can resolve internal names and use DNS
  as a low-bandwidth side channel even without `--lan`. That is inherent to
  forwarding to the host's resolver and is not fixable in the guest; it belongs
  in the README's threat model next to the existing "defence in depth, not a
  host-enforced boundary" note.
- The loopback-shadow consequence of the old bug is worth stating plainly: a
  misconfigured `dns_host` doesn't fail closed, it **redirects guest traffic at
  the host's loopback** — the one place `--no-map-gw` exists to keep the guest
  away from. It was limited to UDP/TCP 53 and 853 because `is_dns_flow()` gates
  on those ports, but the shape of the failure is "isolation knob silently
  inverted", not "network down".

### Guest DNS plumbing

- With `useNetworkd = true` and no `systemd-resolved`, `/etc/resolv.conf` is
  written by **openresolv**, via `networking.localCommands` →
  `network-local-commands.service` (`nixos/modules/tasks/network-interfaces.nix`),
  which the networkd module orders after `systemd-networkd.service`. So
  `networking.nameservers` does land in `/etc/resolv.conf` — but by way of a
  oneshot unit, not statically in the store.
- `cbox-setup` (which gates `/run/cbox/ready`) has **no ordering relationship**
  to `network-local-commands`. Nothing observed this being a problem, but a
  session that raced it would hand the agent a box where c-ares fell back to its
  built-in `127.0.0.1` default and everything failed with `ECONNREFUSED`.
  c-ares reads `resolv.conf` once at channel init, so the process would stay
  broken for its whole life. If this ever shows up, add
  `after = [ "network-local-commands.service" ]` to `cbox-setup`.
- A boot-time DNS smoke test in `cbox-setup` would have caught this class of bug
  immediately, and `cbox-setup` already fails closed. Not added: it would make
  booting a box depend on working egress, which is the wrong trade for a tool
  you might want offline. A non-fatal warning printed by `cbox` after the
  ready-wait would be the better version if someone wants it.

### Bun vs Node, in the guest

- Because Claude Code is Bun and Bun uses **c-ares on Linux**, the box's DNS
  behaviour differs from Node's in ways that will bite again:
  `/etc/hosts` and anything else NSS-based is **ignored** by `claude`, while
  `curl`, `getent` and Node all honour it. Any future "works in the shell, not
  in Claude" DNS report starts here.
- Equally: Claude Code surfaces raw DNS rcodes (`ENOTIMP`, `EREFUSED`,
  `ESERVFAIL`) that a Node program would never show you. Treat any of those in
  "Unable to connect to API (X)" as *"the resolver answered, hostilely"* — never
  as a TLS, proxy or auth problem.

### Build / packaging

- `nixpkgs.claude-code` is `fetchurl` of a prebuilt binary + `autoPatchelfHook`,
  with `dontStrip = true` (stripping makes the Bun runtime run instead of the
  embedded app). It is not `buildNpmPackage`, so there is no `cli.js` to patch
  and no Node in the closure to blame.
- `kitty.terminfo` is pinned in `systemPackages` rather than
  `environment.enableAllTerminfo` because the latter builds a dozen terminal
  emulators and any single build failure takes `system-path` — and the whole box
  — with it. The existing comment in `guest.nix` is accurate; leave it.

---

## 4. Status / what is still unverified

**The DNS bug and its fix are confirmed** against a real passt from nixpkgs
(`passt-2026_07_16.090d739`), by `test/passt-dns/probe.sh` — broken flags
forward to `0.0.0.0:53` and the guest gets nothing; the current flags forward to
the host resolver and the guest gets an answer; and with a NotImp server on the
host loopback the guest sees `rcode=4`, which c-ares reports as `ENOTIMP`.

Still not verified:

- **Nothing has been booted.** No KVM and no `/dev/vhost-vsock` were available,
  so virtiofsd uid translation, fw_cfg credential delivery, the vsock sshd, the
  `/work/.git` read-only bind and the guest's own networking (that
  `169.254.1.1` is actually routed out of the guest, that `cbox-egress` passes
  it, that `/etc/resolv.conf` lands correctly) were read but not exercised. The
  probe tests passt in isolation; it says nothing about the guest side.
- The identity of whatever answered `NOTIMP` on the reporting host's
  `127.0.0.1:53` is unknown and deliberately not chased. The probe shows the
  *class* of listener that produces this symptom, not which one it was.
- The probe pins its own addressing with `-a`/`-n`/`-g` and passes
  `--runas 0:0`, neither of which cbox does. Those affect how passt picks its
  outbound interface, not the DNS decision under test, but they are a
  difference from the real invocation.

### Running the probe in a box with no nix

A container tier with a read-only `/nix` can still get one: `unshare --user
--map-root-user --mount`, bind a writable directory over `/nix`, copy the seed
closure in, and nix is a normal single-user store from there (this is what
h5i's `h5i-box-entrypoint-nix` does, and it works on demand too). Three
root-owned lock files in the seed (`var/nix/gc.lock`, `var/nix/db/reserved`,
`var/nix/db/big-lock`) cannot be read by a namespace that maps only one uid;
they are empty placeholders, so ignore the copy errors and `touch` them.

Two things then bite behind an egress proxy: `channels.nixos.org` is usually
blocked, so the default flake registry lookup fails — pass
`--option flake-registry ""` and a full `github:NixOS/nixpkgs/nixos-unstable#…`
URL instead of `nixpkgs#…`. And binding that store over `/nix` hides anything
else that lived there, so put the tools you still need on `PATH` by absolute
path first.
