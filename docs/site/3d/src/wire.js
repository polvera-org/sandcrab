// One wire = one guest protocol. Host and sandbox talk over a single cable
// (vsock / unix socket, same protocol on every backend): a request card travels
// host → jar, the crab works, a result cube travels back.
// One idea: "one wire, one protocol, every backend."
// Phases (12 s, two exchanges): request 1.6 → work 1.4 → result 1.6 → ack 1.4, ×2.
// Motion budget: one packet at a time, crab work pose, host LED flash on arrival.
import { THREE, createKit, PALETTE as C, ease, clamp01, loopLength, timeline } from './kit.js';
import { createJarBody } from './jar.js';

const ONE = [{ name: 'request', d: 1.6 }, { name: 'work', d: 1.4 }, { name: 'result', d: 1.6 }, { name: 'ack', d: 1.4 }];
const PHASES = [...ONE, ...ONE];

export const wireMeta = {
  title: 'One wire',
  fallback: 'one-wire',
  alt: 'A host server connected to a crab in a jar by a single cable carrying messages',
  loop: loopLength(PHASES),
  still: 0.9,
  phases: PHASES,
  view: { target: [0, 0.5, 0.1], radius: 2.6, yaw: 0.3, pitch: 0.5, fraction: 0.84, sway: 0.05 },
};

export function createWire(opts = {}) {
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.slab(5.4, 2.6, 0.3));

  // host: small teal server tower with vents and a status LED
  const host = new THREE.Group();
  host.position.set(-1.95, 0, -0.1);
  const tower = k.mesh(new THREE.BoxGeometry(0.75, 1.35, 0.85).translate(0, 0.675, 0), k.toon(C.teal700));
  host.add(tower);
  const mVent = k.toon(C.teal500);
  for (let i = 0; i < 3; i++) {
    const vent = k.mesh(new THREE.BoxGeometry(0.5, 0.05, 0.02), mVent, { outline: false });
    vent.position.set(0, 1.12 - i * 0.12, 0.43);
    host.add(vent);
  }
  const mLed = k.toon(C.teal500, { emissive: new THREE.Color('#7FD3E0'), emissiveIntensity: 0.2 });
  const led = k.mesh(new THREE.BoxGeometry(0.1, 0.1, 0.03), mLed, { outline: false });
  led.position.set(-0.22, 0.25, 0.43);
  host.add(led);
  const port = k.mesh(new THREE.BoxGeometry(0.06, 0.16, 0.2), k.toon(C.ink), { outline: false });
  port.position.set(0.39, 0.35, 0.15);
  host.add(port);
  g.add(host);

  // sandbox: crab in a sealed jar
  const jar = createJarBody({ scale: 0.72, sealed: true, crab: { pose: 'idle' } });
  jar.object3d.position.set(1.85, 0, -0.05);
  g.add(jar.object3d);

  // the one cable, lying in a gentle curve on the sand
  const curve = new THREE.CatmullRomCurve3([
    new THREE.Vector3(-1.53, 0.35, 0.05),
    new THREE.Vector3(-1.2, 0.07, 0.35),
    new THREE.Vector3(-0.2, 0.05, 0.6),
    new THREE.Vector3(0.6, 0.05, 0.5),
    new THREE.Vector3(1.12, 0.14, 0.15),
  ]);
  g.add(k.mesh(new THREE.TubeGeometry(curve, 48, 0.045, 5), k.toon(C.ink), { outline: false, cast: true }));
  const plug = k.mesh(new THREE.CylinderGeometry(0.07, 0.07, 0.14, 10).rotateZ(Math.PI / 2), k.toon(C.teal700));
  plug.position.set(1.13, 0.14, 0.15);
  g.add(plug);

  // packets: request = paper card with teal header, result = teal output cube
  const req = new THREE.Group();
  req.add(k.mesh(new THREE.BoxGeometry(0.3, 0.2, 0.05), k.toon(C.paper)));
  const hdr = k.mesh(new THREE.BoxGeometry(0.3, 0.06, 0.055), k.toon(C.teal500), { outline: false });
  hdr.position.y = 0.07;
  req.add(hdr);
  g.add(req);
  const res = k.mesh(new THREE.BoxGeometry(0.2, 0.2, 0.2), k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.45 }));
  g.add(res);

  const p = new THREE.Vector3(), tan = new THREE.Vector3();
  function place(obj, s, lift) {
    curve.getPointAt(s, p);
    curve.getTangentAt(s, tan);
    obj.position.set(p.x, p.y + lift, p.z);
    obj.rotation.set(0, Math.atan2(-tan.z, tan.x), 0);
  }

  function update(t, dt) {
    const L = wireMeta.loop;
    const u = ((t % L) + L) % L;
    const tl = timeline(PHASES, u);
    req.visible = tl.phase === 'request';
    res.visible = tl.phase === 'result';
    if (req.visible) place(req, ease(tl.p), 0.17);
    if (res.visible) place(res, 1 - ease(tl.p), 0.15);
    jar.crab.setPose(tl.phase === 'work' ? 'work' : 'idle');
    jar.update(t, dt);
    // host LED flashes as a result arrives, then decays
    const flash = tl.phase === 'ack' ? Math.exp(-tl.p * 3) : 0;
    mLed.emissiveIntensity = 0.2 + 1.4 * flash;
  }

  return { object3d: g, meta: wireMeta, jar, update, dispose() { jar.dispose(); k.dispose(); } };
}
