#!/usr/bin/env node
// A "guest" for passt that is just a unix socket client.
//
// passt's --socket mode speaks qemu's `-netdev stream` framing: a 4-byte
// big-endian length followed by one raw Ethernet frame. Nothing about that
// needs a VM, KVM, or a tap device -- so this is enough to send a real DNS
// query from the guest side of a real passt and watch where passt sends it.
//
// usage: fakeguest.js SOCKET [QNAME]
const net = require('net');

const SOCK = process.argv[2];
const QNAME = process.argv[3] || 'api.anthropic.com';

const GUEST_MAC = Buffer.from([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
const PASST_MAC = Buffer.from([0x9a, 0x55, 0x9a, 0x55, 0x9a, 0x55]);
const GUEST_IP = [192, 0, 2, 2];
const DNS_IP = [169, 254, 1, 1]; // what the cbox guest has as its only nameserver
const SPORT = 5555;

function ip16(buf) { // one's-complement sum
  let sum = 0;
  for (let i = 0; i + 1 < buf.length; i += 2) sum += buf.readUInt16BE(i);
  if (buf.length % 2) sum += buf[buf.length - 1] << 8;
  while (sum >> 16) sum = (sum & 0xffff) + (sum >>> 16);
  return (~sum) & 0xffff;
}

function dnsQuery(name) {
  const labels = name.split('.').filter(Boolean);
  const qlen = labels.reduce((n, l) => n + 1 + l.length, 0) + 1;
  const b = Buffer.alloc(12 + qlen + 4);
  b.writeUInt16BE(0x1234, 0); // id
  b.writeUInt16BE(0x0100, 2); // standard query, RD
  b.writeUInt16BE(1, 4);      // qdcount
  let o = 12;
  for (const l of labels) { b.writeUInt8(l.length, o++); b.write(l, o); o += l.length; }
  b.writeUInt8(0, o++);
  b.writeUInt16BE(1, o); o += 2; // A
  b.writeUInt16BE(1, o); o += 2; // IN
  return b;
}

function udp4(src, dst, sport, dport, payload) {
  const udp = Buffer.alloc(8 + payload.length);
  udp.writeUInt16BE(sport, 0);
  udp.writeUInt16BE(dport, 2);
  udp.writeUInt16BE(udp.length, 4);
  payload.copy(udp, 8);
  // UDP checksum over the pseudo-header. passt does validate this.
  const pseudo = Buffer.concat([
    Buffer.from(src), Buffer.from(dst),
    Buffer.from([0, 17]), Buffer.from([udp.length >> 8, udp.length & 0xff]),
    udp,
  ]);
  let ck = ip16(pseudo);
  if (ck === 0) ck = 0xffff;
  udp.writeUInt16BE(ck, 6);

  const ip = Buffer.alloc(20 + udp.length);
  ip.writeUInt8(0x45, 0);
  ip.writeUInt16BE(ip.length, 2);
  ip.writeUInt16BE(0x4321, 4); // id
  ip.writeUInt8(64, 8);        // ttl
  ip.writeUInt8(17, 9);        // udp
  Buffer.from(src).copy(ip, 12);
  Buffer.from(dst).copy(ip, 16);
  ip.writeUInt16BE(ip16(ip.subarray(0, 20)), 10);
  udp.copy(ip, 20);

  return Buffer.concat([PASST_MAC, GUEST_MAC, Buffer.from([0x08, 0x00]), ip]);
}

const RCODE = ['NOERROR', 'FORMERR', 'SERVFAIL', 'NXDOMAIN', 'NOTIMP', 'REFUSED'];

function describe(frame) {
  if (frame.length < 14 || frame.readUInt16BE(12) !== 0x0800) return null;
  const ip = frame.subarray(14);
  const ihl = (ip[0] & 0x0f) * 4;
  const src = Array.from(ip.subarray(12, 16)).join('.');
  if (ip[9] !== 17) return `  <- ${src} proto ${ip[9]}`;
  const udp = ip.subarray(ihl);
  const sport = udp.readUInt16BE(0);
  const dns = udp.subarray(8);
  if (sport !== 53 || dns.length < 12) return `  <- ${src}:${sport} udp`;
  const flags = dns.readUInt16BE(2);
  const rc = flags & 0x0f;
  const ancount = dns.readUInt16BE(6);
  return `  <- DNS reply from ${src}:53  rcode=${rc} (${RCODE[rc] || '?'})  answers=${ancount}`;
}

const sock = net.connect(SOCK);
let buf = Buffer.alloc(0);

sock.on('connect', () => {
  const frame = udp4(GUEST_IP, DNS_IP, SPORT, 53, dnsQuery(QNAME));
  const len = Buffer.alloc(4);
  len.writeUInt32BE(frame.length, 0);
  sock.write(Buffer.concat([len, frame]));
  console.log(`  -> DNS query for ${QNAME} to ${DNS_IP.join('.')}:53 (from ${GUEST_IP.join('.')}:${SPORT})`);
});

sock.on('data', (d) => {
  buf = Buffer.concat([buf, d]);
  while (buf.length >= 4) {
    const n = buf.readUInt32BE(0);
    if (buf.length < 4 + n) break;
    const frame = buf.subarray(4, 4 + n);
    buf = buf.subarray(4 + n);
    const line = describe(frame);
    if (line) console.log(line);
  }
});

sock.on('error', (e) => console.log(`  !! socket: ${e.message}`));
setTimeout(() => { console.log('  (done waiting)'); sock.destroy(); process.exit(0); }, 4000);
