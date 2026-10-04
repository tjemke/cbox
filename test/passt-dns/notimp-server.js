#!/usr/bin/env node
// Stands in for "whatever is listening on the cbox host's 127.0.0.1:53".
// Answers every query with RCODE 4 (NotImp) -- what a non-recursive or
// non-DNS-but-port-53 listener plausibly does, and the rcode c-ares surfaces
// to Claude Code as ENOTIMP.
const dgram = require('dgram');
const [addr, port] = [process.argv[2] || '127.0.0.1', Number(process.argv[3] || 53)];
const s = dgram.createSocket('udp4');
s.on('message', (msg, rinfo) => {
  const r = Buffer.from(msg.subarray(0, Math.min(msg.length, 512)));
  r.writeUInt16BE((r.readUInt16BE(2) | 0x8000) & 0xfff0 | 4, 2); // QR=1, RCODE=4
  r.writeUInt16BE(0, 6); // ancount
  s.send(r, rinfo.port, rinfo.address);
  console.error(`  [notimp-server] answered ${rinfo.address}:${rinfo.port} with RCODE 4`);
});
s.bind(port, addr, () => console.error(`  [notimp-server] listening on ${addr}:${port}`));
