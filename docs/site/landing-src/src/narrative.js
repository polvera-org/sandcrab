// The narrative film: ONE fixed canvas, ONE renderer, ONE crab for the whole page.
//
// Every section with [data-station] is a keyframe on a single scroll timeline. Between two
// keyframes the crab walks (leg cycle) from one side of the screen to the other while the
// outgoing props sink, shrink or dissolve and the incoming ones rise, grow or assemble. Inside a
// keyframe ("hold") the scroll scrubs the station's own little story through h = 0..1 (the molt,
// the stacking blocks, the tool cards, the sandcastle). Everything is a pure function of
// (scroll, clock), so scrolling back plays it in reverse. Props are built once and never disposed
// while the page lives.
//
// Placement: each station measures its props' bounding box once (in camera space), and the
// orthographic frustum is shifted and zoomed so that box lands centred in the empty `.aside`
// column of its section. The text column is never under the crab.
import { THREE, createKit, PALETTE as C, ease, clamp01, lerp, setOutlineWidth, rng } from './kit3d/src/kit.js';
import { createCrab } from './kit3d/src/crab.js';
import { TOOLS, drawGlyph } from './kit3d/src/toolbelt.js';

const TAU = Math.PI * 2;
const CS = 0.5; // crab scale, constant for the whole film
const win = (x, a, b) => ease(clamp01((x - a) / (b - a)));
const bump = (x, a, b) => Math.sin(Math.PI * clamp01((x - a) / (b - a)));
const backOut = (x) => { const c = 1.5; const y = x - 1; return 1 + (c + 1) * y * y * y + c * y * y; };
const lerp3 = (a, b, t) => [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t)];
const vec = (s) => (Array.isArray(s) ? s : [s, s, s]);

// ── 3D palette tokens, light and dark (the page theme toggle drives these) ──
const THEME = {
  light: { shadow: 0.14, hemi: 1.5, sun: 2.2, sand: '#ECEAE2', sandDeep: '#C9C5B8', rim: C.ink, water: C.teal500 },
  dark: { shadow: 0.36, hemi: 1.25, sun: 2.0, sand: '#CBC5B4', sandDeep: '#9F998A', rim: '#A9CBD2', water: '#2F8FA2' },
};
function isDark() {
  const t = document.documentElement.getAttribute('data-theme');
  if (t === 'dark') return true;
  if (t === 'light') return false;
  return matchMedia('(prefers-color-scheme: dark)').matches;
}

/** Reduced-motion rest pose (h) for each station: the most telling still frame. */
const REST = { hero: 0, turn: 0.72, sdk: 1, workspaces: 0.25, backends: 0.5, tools: 0.35, protocol: 0.5, crabagent: 1, start: 1 };

export function startFilm(host, { onFail } = {}) {
  const stationEls = [...document.querySelectorAll('[data-station]')];
  if (!stationEls.length) return null;

  // ── renderer ──
  let renderer;
  try {
    renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true, powerPreference: 'high-performance' });
  } catch (err) { onFail?.(err); return null; }
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.shadowMap.enabled = true;
  renderer.shadowMap.type = THREE.PCFShadowMap;
  renderer.setClearColor(0x000000, 0);
  const canvas = renderer.domElement;
  canvas.setAttribute('aria-hidden', 'true');
  host.appendChild(canvas);
  canvas.addEventListener('webglcontextlost', (e) => { e.preventDefault(); onFail?.(new Error('context lost')); });

  const scene = new THREE.Scene();
  const YAW = 0.42, PITCH = 0.5;
  const camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.1, 200);
  const dir = new THREE.Vector3(Math.cos(PITCH) * Math.sin(YAW), Math.sin(PITCH), Math.cos(PITCH) * Math.cos(YAW));
  camera.position.copy(dir).multiplyScalar(40);
  camera.lookAt(0, 0, 0);
  camera.updateMatrixWorld(true);
  const hemi = new THREE.HemisphereLight('#FFF6E6', '#BFA77C', 1.5);
  const sun = new THREE.DirectionalLight('#FFFFFF', 2.2);
  sun.castShadow = true;
  sun.shadow.mapSize.set(2048, 2048);
  sun.shadow.radius = 3;
  sun.shadow.bias = -0.001;
  sun.shadow.normalBias = 0.04;
  const ly = YAW - 0.8;
  sun.position.set(12 * Math.sin(ly), 16, 12 * Math.cos(ly));
  Object.assign(sun.shadow.camera, { left: -5, right: 5, top: 5, bottom: -5, near: 1, far: 60 });
  sun.shadow.camera.updateProjectionMatrix();
  scene.add(hemi, sun, sun.target);
  const catcherMat = new THREE.ShadowMaterial({ opacity: 0.14 });
  const catcher = new THREE.Mesh(new THREE.PlaneGeometry(60, 60), catcherMat);
  catcher.rotation.x = -Math.PI / 2;
  catcher.receiveShadow = true;
  scene.add(catcher);

  const k = createKit();
  const world = new THREE.Group();
  scene.add(world);
  const mSand = k.toon(C.sand200);
  const mDeep = k.toon(C.sandDeep);
  const mTeal = k.toon(C.teal500);
  const mTealD = k.toon(C.teal700);
  const mPaper = k.toon(C.paper);
  const mInk = k.toon(C.ink);
  const mRock = k.toon(C.rock);
  const glassMats = [];
  const rimLines = [];
  const glass = (o) => { const m = k.glass(o); glassMats.push({ m, op: m.uniforms.uOpacity.value, rim: m.uniforms.uRimOpacity.value }); return m; };

  // ── the one crab ──
  const crab = createCrab({ scale: CS, pose: 'idle' });
  world.add(crab.object3d);

  // ── props registry ──
  // mode 'grow': scale pops in (back-out) and rises from the sand; 'sink': no scaling, the prop
  // sinks into the sand / rises out of it and its glass fades; 'fade': the after() hook handles it.
  const props = {};
  const prop = (name, obj, opt = {}) => { world.add(obj); props[name] = { obj, mode: 'grow', rise: 0.5, ...opt }; return obj; };

  // ground disc, travels with the crab and changes shape between stations
  const disc = k.mesh(new THREE.CylinderGeometry(1, 1, 0.25, 64, 1).translate(0, -0.125, 0), mSand, { receive: true });
  prop('disc', disc, { rise: 0 });

  // ── jar (hero sandbox; reappears open in Backends) ──
  const jar = new THREE.Group();
  {
    const PROFILE = [[0, 0], [0.86, 0], [0.98, 0.06], [1.02, 0.2], [1.02, 1.55], [0.96, 1.72], [0.84, 1.82], [0.8, 1.86], [0.8, 2.02]];
    const bed = k.mesh(new THREE.CylinderGeometry(0.9, 0.94, 0.14, 28).translate(0, 0.07, 0), mSand, { outline: false, receive: true });
    const geo = k.geo(new THREE.LatheGeometry(PROFILE.map(([x, y]) => new THREE.Vector2(x, y)), 32));
    const back = new THREE.Mesh(geo, glass({ back: true, opacity: 0.08, rimOpacity: 0.25 }));
    back.renderOrder = 1;
    const front = new THREE.Mesh(geo, glass({ opacity: 0.12, rimOpacity: 0.95 }));
    front.renderOrder = 2;
    const mouthMat = k.basic(C.ink, { transparent: true });
    rimLines.push(mouthMat);
    const mouth = new THREE.Mesh(k.geo(new THREE.TorusGeometry(0.8, 0.022, 4, 32).rotateX(Math.PI / 2)), mouthMat);
    mouth.position.y = 2.02;
    const lid = new THREE.Group();
    const cap = k.mesh(new THREE.CylinderGeometry(0.88, 0.88, 0.3, 32), mTeal);
    cap.position.y = 0.15;
    lid.add(cap);
    for (let i = 0; i < 3; i++) {
      const r = k.mesh(new THREE.TorusGeometry(0.89, 0.025, 3, 32).rotateX(Math.PI / 2), mTealD, { outline: false });
      r.position.y = 0.06 + i * 0.09;
      lid.add(r);
    }
    jar.add(bed, back, front, mouth, lid);
    jar.userData = { lid, mouthMat, glass: glassMats.slice(-2) };
  }
  prop('jar', jar, {
    mode: 'sink', rise: 2.6,
    after(st) {
      const { lid, mouthMat } = jar.userData;
      const a = st ? clamp01(st.p) : 0;
      for (const g of jar.userData.glass) { g.m.uniforms.uOpacity.value = g.op * a; g.m.uniforms.uRimOpacity.value = g.rim * a; }
      mouthMat.opacity = a;
      const open = st?.lid ?? 0;
      lid.visible = !st?.lidOff;
      lid.position.y = 1.86 + open * 0.75;
      lid.rotation.y = open * Math.PI * 1.5;
    },
  });

  // ── molt: output cube, husk (the old exoskeleton), sand grains ──
  const cubeMat = k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.45 });
  const cube = k.mesh(new THREE.BoxGeometry(0.3, 0.3, 0.3).translate(0, 0.15, 0), cubeMat);
  prop('cube', cube, { after(st) { if (st) cubeMat.emissiveIntensity = 0.35 + 0.5 * (st.glow ?? 0); } });

  const husk = createCrab({ scale: 1, shell: 'husk', split: true, detail: 'high' });
  husk.update(1, 0);
  prop('husk', husk.object3d, {
    mode: 'fade', rise: 0.2,
    after(st) {
      if (!st) return;
      husk.setOpacity(clamp01(st.p));
      husk.crack(st.crack ?? 0);
    },
  });

  const N = 46;
  const R = rng(11);
  const grains = new THREE.InstancedMesh(k.geo(new THREE.BoxGeometry(0.05, 0.05, 0.05)), k.toon(C.sandDeep), N);
  const seeds = Array.from({ length: N }, () => ({
    x: (R() - 0.5) * 0.9, y: 0.12 + R() * 0.4, z: (R() - 0.5) * 0.7,
    dx: 0.2 + R() * 0.5, dy: 0.08 + R() * 0.3, d: R() * 0.35, rot: R() * 6,
  }));
  const m4 = new THREE.Matrix4(), q4 = new THREE.Quaternion(), e4 = new THREE.Euler(), v4 = new THREE.Vector3(), s4 = new THREE.Vector3();
  prop('grains', grains, {
    mode: 'fade', rise: 0,
    after(st) {
      if (!st) return;
      const gp = st.gp ?? 0;
      seeds.forEach((s, i) => {
        const p = clamp01((gp - s.d) / (1 - s.d));
        const ep = ease(p);
        v4.set(s.x - s.dx * ep, Math.max(0.02, s.y + s.dy * Math.sin(Math.PI * p) - s.y * ep), s.z - 0.15 * ep);
        const sc = p <= 0 ? 0.0001 : Math.max(0.0001, (1 - p) * 1.2);
        s4.setScalar(sc);
        e4.set(s.rot * p, s.rot, 0);
        q4.setFromEuler(e4);
        m4.compose(v4, q4, s4);
        grains.setMatrixAt(i, m4);
      });
      grains.instanceMatrix.needsUpdate = true;
    },
  });

  // ── SDK: labelled blocks that stack as the code reads, plus the file that stays ──
  function labelTex(text, bg, fg, w = 256, h = 140) {
    const c = document.createElement('canvas');
    c.width = w; c.height = h;
    const x = c.getContext('2d');
    x.fillStyle = bg; x.fillRect(0, 0, w, h);
    x.fillStyle = fg;
    x.font = `600 ${Math.round(h * 0.3)}px "JetBrains Mono", ui-monospace, Menlo, monospace`;
    x.textAlign = 'center'; x.textBaseline = 'middle';
    x.fillText(text, w / 2, h / 2 + 2);
    const t = k.tex(new THREE.CanvasTexture(c));
    t.colorSpace = THREE.SRGBColorSpace;
    t.anisotropy = 4;
    return t;
  }
  const BLOCKS = [['builder', C.teal700], ['workspace', C.teal500], ['spawn', C.teal700], ['bash', C.teal500]];
  const BH = 0.34;
  const blockGeo = new THREE.BoxGeometry(0.78, BH, 0.5).translate(0, BH / 2, 0);
  const blockTexs = [];
  BLOCKS.forEach(([label, col], i) => {
    const side = k.toon(col);
    const tex = labelTex(label, col, '#FFFCF6');
    blockTexs.push(tex);
    const face = k.toon('#FFFFFF', { map: tex });
    prop(`block${i}`, k.mesh(blockGeo, [side, side, side, side, face, side]), { rise: 0 });
  });
  const fileTex = (() => {
    const c = document.createElement('canvas');
    c.width = 128; c.height = 160;
    const x = c.getContext('2d');
    x.fillStyle = C.paper; x.fillRect(0, 0, 128, 160);
    x.strokeStyle = C.ink; x.lineWidth = 6; x.strokeRect(3, 3, 122, 154);
    x.fillStyle = C.ink;
    x.font = '700 64px "JetBrains Mono", ui-monospace, monospace';
    x.textAlign = 'center'; x.textBaseline = 'middle';
    x.fillText('42', 64, 64);
    x.fillStyle = C.teal500;
    for (let i = 0; i < 3; i++) x.fillRect(22, 108 + i * 14, 84 - i * 18, 6);
    const t = k.tex(new THREE.CanvasTexture(c));
    t.colorSpace = THREE.SRGBColorSpace;
    return t;
  })();
  const fileFace = k.toon('#FFFFFF', { map: fileTex });
  const file = k.mesh(new THREE.BoxGeometry(0.3, 0.38, 0.03).translate(0, 0.19, 0), [mPaper, mPaper, mPaper, mPaper, fileFace, mPaper]);
  prop('file', file, { rise: 0.25 });

  // ── Workspaces: the burrow (cutaway), crates and pebbles that persist ──
  const BS = 0.68;
  const burrow = new THREE.Group();
  const crateMats = [];
  {
    const W = 4.2, D = 3.0, H = 1.8;
    const cav = { x0: -1.45, x1: 1.45, y0: -1.55, y1: -0.3, z0: -0.25 };
    const block = (x0, x1, y0, y1, z0, z1, m = mSand, receive = false) => {
      const geo = new THREE.BoxGeometry(x1 - x0, y1 - y0, z1 - z0).translate((x0 + x1) / 2, (y0 + y1) / 2, (z0 + z1) / 2);
      burrow.add(k.mesh(geo, m, { receive }));
    };
    block(-W / 2, W / 2, cav.y1, 0, -D / 2, D / 2, mSand, true);
    block(-W / 2, W / 2, -H, cav.y0, -D / 2, D / 2);
    block(-W / 2, cav.x0, cav.y0, cav.y1, -D / 2, D / 2);
    block(cav.x1, W / 2, cav.y0, cav.y1, -D / 2, D / 2);
    block(cav.x0, cav.x1, cav.y0, cav.y1, -D / 2, cav.z0, mDeep);
    const hole = new THREE.Mesh(k.geo(new THREE.CircleGeometry(0.2, 24).rotateX(-Math.PI / 2)), k.basic(C.ink, { transparent: true, opacity: 0.5, depthWrite: false }));
    hole.scale.z = 0.8;
    hole.position.set(0.05, 0.004, 0.75);
    burrow.add(hole);
    const S = 0.44;
    const crateGeo = new THREE.BoxGeometry(S, S, S);
    const bandGeo = new THREE.BoxGeometry(S + 0.02, 0.07, S + 0.02);
    const fy = cav.y0 + S / 2;
    [[-0.95, fy, 0.55], [-0.45, fy, 0.85], [-0.85, fy + S, 0.6], [0.05, fy, 0.75]].forEach(([x, y, z]) => {
      const m = k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.12 });
      const c = k.mesh(crateGeo, m);
      c.position.set(x, y, z);
      c.add(k.mesh(bandGeo, mTealD, { outline: false }));
      burrow.add(c);
      crateMats.push(m);
    });
    const folder = k.mesh(new THREE.BoxGeometry(0.5, 0.3, 0.36), mTealD, { receive: true });
    folder.position.set(0.75, cav.y0 + 0.15, 0.9);
    burrow.add(folder);
    for (let i = 0; i < 3; i++) {
      const sheet = k.mesh(new THREE.BoxGeometry(0.03, 0.26, 0.3), mPaper, { outline: i === 0 });
      sheet.position.set(0.61 + i * 0.1, cav.y0 + 0.36, 0.9);
      burrow.add(sheet);
    }
    // pebbles: in the chamber and on the surface, they persist too
    const pg = new THREE.DodecahedronGeometry(0.1, 0);
    [[1.15, cav.y0 + 0.06, 0.6], [0.45, cav.y0 + 0.05, 1.1], [-1.6, 0.04, 1.1], [1.7, 0.04, -0.6], [-0.4, 0.04, -1.0]].forEach(([x, y, z], i) => {
      const r = k.mesh(pg, mRock, { receive: true });
      r.position.set(x, y, z);
      r.scale.set(1.3, 0.6, 1);
      r.rotation.y = i;
      burrow.add(r);
    });
  }
  prop('burrow', burrow, {
    rise: 1.6,
    after(st) { if (st) crateMats.forEach((m, i) => { m.emissiveIntensity = 0.12 + 0.8 * (st.flash?.[i] ?? 0); }); },
  });
  const ghosts = [0, 1].map(() => createCrab({ scale: CS, shell: 'husk', detail: 'low' }));
  ghosts.forEach((g, i) => {
    g.update(0.6 + i, 0);
    prop(`ghost${i}`, g.object3d, { mode: 'fade', rise: 0, after(st) { if (st) g.setOpacity(clamp01(st.p) * 0.8); } });
  });

  // ── Backends: grey crabs sharing the open jar; the sealed glass sphere ──
  const greys = [0, 1].map((i) => createCrab({ scale: 0.13, shell: 'grey', detail: 'low', phase: i * 0.7 }));
  const greyPool = new THREE.Group();
  greys.forEach((g) => greyPool.add(g.object3d));
  prop('greys', greyPool, {
    rise: 0.2,
    after(st) {
      if (!st) return;
      const c = st.clock ?? 0;
      greys.forEach((g, i) => {
        const a = c * 0.6 + i * Math.PI;
        g.object3d.position.set(Math.cos(a) * 0.17, 0, Math.sin(a) * 0.12);
        g.object3d.rotation.y = -a;
        g.setWalk(0.8);
        g.update(c, 0);
      });
    },
  });
  const sphere = new THREE.Group();
  {
    const base = k.mesh(new THREE.CylinderGeometry(0.5, 0.58, 0.16, 32).translate(0, 0.08, 0), mTealD);
    const band = k.mesh(new THREE.TorusGeometry(0.66, 0.045, 6, 40).rotateX(Math.PI / 2), mTeal);
    band.position.y = 0.72;
    const sg = k.geo(new THREE.SphereGeometry(0.68, 32, 20));
    const back = new THREE.Mesh(sg, glass({ back: true, opacity: 0.1, rimOpacity: 0.3 }));
    back.renderOrder = 1;
    const front = new THREE.Mesh(sg, glass({ opacity: 0.14, rimOpacity: 0.95 }));
    front.renderOrder = 2;
    back.position.y = front.position.y = 0.76;
    const bed = k.mesh(new THREE.CylinderGeometry(0.42, 0.42, 0.06, 24).translate(0, 0.19, 0), mSand, { outline: false });
    const kernel = k.mesh(new THREE.OctahedronGeometry(0.17, 0), k.toon(C.teal500, { emissive: new THREE.Color('#7FD3E0'), emissiveIntensity: 0.7 }));
    kernel.position.y = 0.55;
    sphere.add(base, bed, kernel, back, front, band);
    sphere.userData.kernel = kernel;
  }
  prop('sphere', sphere, { rise: 0.6, after(st) { if (st) sphere.userData.kernel.rotation.y = (st.clock ?? 0) * 0.8; } });

  // ── Tools: belt + holstered cards on the crab's shell, one card held up in the claw ──
  const toolTexs = TOOLS.map((tool) => {
    const c = document.createElement('canvas');
    c.width = c.height = 128;
    drawGlyph(c.getContext('2d'), 128, tool);
    const t = k.tex(new THREE.CanvasTexture(c));
    t.colorSpace = THREE.SRGBColorSpace;
    return t;
  });
  const belt = new THREE.Group();
  {
    const torus = k.mesh(new THREE.TorusGeometry(1, 0.075, 6, 48).rotateX(Math.PI / 2), mTealD);
    torus.scale.set(1.13, 1, 0.93);
    belt.add(torus);
    belt.userData.mini = TOOLS.map((tool, i) => {
      const a = -0.95 + (i / (TOOLS.length - 1)) * 1.9;
      const m = k.mesh(new THREE.BoxGeometry(0.2, 0.2, 0.03), [mPaper, mPaper, mPaper, mPaper, k.toon('#FFFFFF', { map: toolTexs[i] }), mPaper]);
      m.position.set(Math.sin(a) * 1.15, 0, Math.cos(a) * 0.97);
      m.rotation.y = a;
      belt.add(m);
      return m;
    });
  }
  crab.parts.body.add(belt);
  belt.position.y = -0.06;
  props.belt = { obj: belt, mode: 'grow', rise: 0, local: true };
  const holder = new THREE.Group();
  const heldFace = k.toon('#FFFFFF', { map: toolTexs[0] });
  const held = k.mesh(new THREE.BoxGeometry(0.62, 0.62, 0.05), [mPaper, mPaper, mPaper, mPaper, heldFace, mPaper]);
  holder.add(held);
  crab.parts.body.add(holder);
  const jaw = crab.parts.claws[1].jawTop;
  const tip = new THREE.Vector3(), qParent = new THREE.Quaternion(), qWant = new THREE.Quaternion();
  const qFace = new THREE.Quaternion().setFromEuler(new THREE.Euler(-0.12, 0.35, 0, 'YXZ'));
  props.card = {
    obj: holder, mode: 'grow', rise: 0, local: true,
    after(st) {
      if (!st) { belt.userData.mini.forEach((m) => { m.visible = true; }); return; }
      const h = st.h ?? 0;
      const x = clamp01(h) * TOOLS.length;
      const i = Math.min(TOOLS.length - 1, Math.floor(x));
      const f = x - i;
      let rot = 0, show = i;
      if (i > 0 && f < 0.3) {
        const qq = f / 0.3;
        if (qq < 0.5) { rot = ease(qq / 0.5) * Math.PI / 2; show = i - 1; } else rot = (1 - ease((qq - 0.5) / 0.5)) * Math.PI / 2;
      }
      held.rotation.y = rot;
      heldFace.map = toolTexs[show];
      belt.userData.mini.forEach((m, j) => { m.visible = j !== show; });
      // the card sits just above the raised claw's tip, upright and facing the viewer
      world.updateMatrixWorld(true);
      jaw.getWorldPosition(tip);
      crab.parts.body.worldToLocal(tip);
      holder.position.set(tip.x + 0.05, tip.y + 0.44, tip.z);
      world.getWorldQuaternion(qWant).multiply(qFace);
      holder.parent.getWorldQuaternion(qParent);
      holder.quaternion.copy(qParent.invert().multiply(qWant));
    },
  };

  // ── Protocol: host tower, one cable, packets ──
  const tower3d = new THREE.Group();
  {
    const tower = k.mesh(new THREE.BoxGeometry(0.75, 1.35, 0.85).translate(0, 0.675, 0), mTealD);
    tower3d.add(tower);
    for (let i = 0; i < 3; i++) {
      const vent = k.mesh(new THREE.BoxGeometry(0.5, 0.05, 0.02), mTeal, { outline: false });
      vent.position.set(0, 1.12 - i * 0.12, 0.43);
      tower3d.add(vent);
    }
    const ledMat = k.toon(C.teal500, { emissive: new THREE.Color('#7FD3E0'), emissiveIntensity: 0.2 });
    const led = k.mesh(new THREE.BoxGeometry(0.1, 0.1, 0.03), ledMat, { outline: false });
    led.position.set(-0.22, 0.25, 0.43);
    tower3d.add(led);
    tower3d.userData.led = ledMat;
  }
  prop('host', tower3d, { rise: 0.9, after(st) { if (st) tower3d.userData.led.emissiveIntensity = 0.2 + 1.2 * (st.flash ?? 0); } });
  const HOST_X = -1.75, CRAB_PX = 1.25;
  const curve = new THREE.CatmullRomCurve3([
    new THREE.Vector3(HOST_X + 0.38, 0.35, 0.1),
    new THREE.Vector3(HOST_X + 0.75, 0.06, 0.45),
    new THREE.Vector3(-0.2, 0.05, 0.7),
    new THREE.Vector3(0.6, 0.05, 0.6),
    new THREE.Vector3(CRAB_PX - 0.35, 0.08, 0.45),
  ]);
  const cableGeo = k.geo(new THREE.TubeGeometry(curve, 64, 0.045, 6));
  const cable = new THREE.Mesh(cableGeo, k.toon(C.ink));
  cable.castShadow = true;
  const cableCount = cableGeo.index.count;
  prop('cable', cable, {
    mode: 'fade', rise: 0,
    after(st) { if (st) cableGeo.setDrawRange(0, Math.max(6, Math.floor(ease(clamp01(st.p)) * cableCount / 6) * 6)); },
  });
  const packets = new THREE.Group();
  const reqGeo = new THREE.BoxGeometry(0.28, 0.19, 0.05);
  const pk = [0, 1, 2].map((i) => {
    const p = i === 1
      ? k.mesh(new THREE.BoxGeometry(0.18, 0.18, 0.18), cubeMat)
      : k.mesh(reqGeo, mPaper);
    packets.add(p);
    return p;
  });
  const pv = new THREE.Vector3(), tv = new THREE.Vector3();
  prop('packets', packets, {
    mode: 'fade', rise: 0,
    after(st) {
      if (!st) return;
      const run = (st.clock ?? 0) * 0.28 + (st.h ?? 0) * 0.8;
      pk.forEach((p, i) => {
        let s = (run + i / 3) % 1;
        if (i === 1) s = 1 - s; // the result travels back to the host
        s = clamp01(s);
        curve.getPointAt(s, pv);
        curve.getTangentAt(s, tv);
        p.position.set(pv.x, pv.y + 0.16, pv.z);
        p.rotation.set(0, Math.atan2(-tv.z, tv.x), 0);
        p.scale.setScalar(Math.max(0.0001, Math.min(1, st.p * 1.2) * Math.min(1, Math.sin(Math.PI * s) * 4)));
      });
    },
  });

  // ── crabagent: a sandcastle assembled piece by piece, with a page on its front ──
  const CX = 0.55;
  const castle = {};
  {
    const base = k.mesh(new THREE.BoxGeometry(1.3, 0.5, 0.9).translate(0, 0.25, 0), mDeep, { receive: true });
    castle.base = base;
    const tGeo = new THREE.CylinderGeometry(0.21, 0.24, 0.95, 16).translate(0, 0.475, 0);
    castle.towerL = k.mesh(tGeo, mSand);
    castle.towerR = k.mesh(tGeo, mSand);
    const rGeo = new THREE.ConeGeometry(0.28, 0.4, 16).translate(0, 0.2, 0);
    castle.roofL = k.mesh(rGeo, mTeal);
    castle.roofR = k.mesh(rGeo, mTeal);
    const cren = new THREE.Group();
    const cg = new THREE.BoxGeometry(0.16, 0.14, 0.16).translate(0, 0.07, 0);
    for (let i = 0; i < 4; i++) {
      const c = k.mesh(cg, mSand);
      c.position.set(-0.36 + i * 0.24, 0, 0.36);
      cren.add(c);
    }
    castle.cren = cren;
    const pc = document.createElement('canvas');
    pc.width = 192; pc.height = 128;
    const x = pc.getContext('2d');
    x.fillStyle = C.paper; x.fillRect(0, 0, 192, 128);
    x.fillStyle = C.coral; x.fillRect(16, 16, 74, 14);
    x.fillStyle = C.ink;
    for (let i = 0; i < 4; i++) x.fillRect(16, 44 + i * 16, 160 - (i % 2) * 40, 6);
    x.fillStyle = C.teal500; x.fillRect(120, 14, 56, 20);
    x.strokeStyle = C.ink; x.lineWidth = 5; x.strokeRect(2.5, 2.5, 187, 123);
    const ptex = k.tex(new THREE.CanvasTexture(pc));
    ptex.colorSpace = THREE.SRGBColorSpace;
    castle.page = k.mesh(new THREE.BoxGeometry(0.6, 0.4, 0.03).translate(0, 0.2, 0), [mPaper, mPaper, mPaper, mPaper, k.toon('#FFFFFF', { map: ptex }), mPaper]);
    const flag = new THREE.Group();
    const pole = k.mesh(new THREE.CylinderGeometry(0.015, 0.015, 0.45, 6).translate(0, 0.225, 0), mInk, { outline: false });
    const fs = new THREE.Shape();
    fs.moveTo(0, 0); fs.lineTo(0.26, -0.08); fs.lineTo(0, -0.16); fs.closePath();
    const cloth = k.mesh(new THREE.ShapeGeometry(fs), k.toon(C.teal500, { side: THREE.DoubleSide }), { outline: false });
    cloth.position.y = 0.44;
    flag.add(pole, cloth);
    castle.flag = flag;
    castle.flag.userData.cloth = cloth;
  }
  Object.entries(castle).forEach(([n, o]) => prop(`castle_${n}`, o, {
    rise: 0.35,
    after: n === 'flag' ? (st) => { if (st) o.userData.cloth.rotation.y = 0.25 * Math.sin((st.clock ?? 0) * 3); } : undefined,
  }));

  // ── Shore: water at the back, pebbles, the workspace crate still there ──
  const waterMat = k.toon(C.teal500, { transparent: true, opacity: 0.85 });
  const water = new THREE.Group();
  {
    const wg = new THREE.RingGeometry(1.45, 2.45, 64, 1, Math.PI * 0.08, Math.PI * 0.84).rotateX(-Math.PI / 2);
    const w = new THREE.Mesh(k.geo(wg), waterMat);
    w.position.y = 0.012;
    w.receiveShadow = true;
    const foamMat = k.basic(C.paper, { transparent: true, opacity: 0.8, depthWrite: false });
    const foam = new THREE.Mesh(k.geo(new THREE.RingGeometry(1.45, 1.5, 64, 1, Math.PI * 0.08, Math.PI * 0.84).rotateX(-Math.PI / 2)), foamMat);
    foam.position.y = 0.016;
    water.add(w, foam);
    water.rotation.y = Math.PI; // the arc sits behind the crab (−Z)
    water.userData.foam = foam;
  }
  prop('water', water, { rise: 0.05, after(st) { if (st) { const s = 1 + 0.025 * Math.sin((st.clock ?? 0) * 1.4); water.userData.foam.scale.set(s, 1, s); } } });
  const pebbles = new THREE.Group();
  {
    const pg = new THREE.DodecahedronGeometry(0.1, 0);
    [[-1.3, 0.75], [-0.9, 1.25], [1.35, 0.95], [1.65, 0.3], [0.5, 1.5]].forEach(([x, z], i) => {
      const r = k.mesh(pg, mRock, { receive: true });
      r.position.set(x, 0.04, z);
      r.scale.set(1.2 + (i % 2) * 0.4, 0.6, 1);
      r.rotation.y = i * 1.3;
      pebbles.add(r);
    });
    const crate = k.mesh(new THREE.BoxGeometry(0.34, 0.34, 0.34).translate(0, 0.17, 0), k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.25 }));
    crate.position.set(-1.25, 0, 0.2);
    crate.rotation.y = 0.4;
    pebbles.add(crate);
  }
  prop('pebbles', pebbles, { rise: 0.3 });

  // ── Stations ──
  // crab(h, clock) → { pos, ry, pose, pose2, pw, walk, tint:[hex, k] }
  // props[name](h, clock) → { pos, s, ry, p, ...extra } ; absent = hidden at this station
  const at = (x, y, z) => [x, y, z];
  const JS = 1.15; // hero jar scale: holds a crab of scale CS
  const STATIONS = {
    hero: {
      ground: -0.25,
      crab: (h, c) => ({ pos: at(0.08 * Math.sin(c * TAU / 6), 0.14 * JS, 0), walk: 0.5 * Math.abs(Math.cos(c * TAU / 6)) }),
      props: {
        disc: () => ({ s: [1.7, 1, 1.7] }),
        jar: () => ({ s: JS, lid: 0 }),
      },
    },
    turn: {
      ground: -0.25,
      crab: (h) => {
        const sink = win(h, 0.12, 0.28);
        const out = win(h, 0.5, 0.68);
        const tint = h < 0.5 ? [C.husk, win(h, 0.34, 0.48) * 0.55] : ['#FFB49C', 0.5 * (1 - win(h, 0.62, 0.95))];
        const pose2 = h < 0.7 ? 'work' : 'wave';
        const pw = h < 0.7 ? win(h, 0.24, 0.3) * (1 - win(h, 0.44, 0.5)) : win(h, 0.84, 0.94);
        return { pos: at(lerp(-0.4, 0.8, out), 0.14 * JS * (1 - sink), 0), walk: bump(h, 0.5, 0.68), tint, pose2, pw };
      },
      props: {
        disc: () => ({ pos: at(0.2, 0, 0), s: [2.0, 1, 1.6] }),
        jar: (h) => ({ pos: at(-0.4, 0, 0), s: JS, lid: win(h, 0.02, 0.14), p: 1 - win(h, 0.12, 0.28) }),
        cube: (h) => ({ pos: at(0.45, 0, 0.75), ry: 0.3, p: win(h, 0.28, 0.42), glow: bump(h, 0.28, 0.5) }),
        husk: (h) => {
          const a = win(h, 0.36, 0.42) * (1 - 0.6 * win(h, 0.62, 0.74)) * (1 - win(h, 0.8, 0.94));
          return { pos: at(-0.4, -0.15 * win(h, 0.8, 0.96), 0), s: CS * 1.04, p: h > 0.97 ? 0 : a, crack: win(h, 0.44, 0.6) * 0.55 };
        },
        grains: (h) => ({ pos: at(-0.4, 0, 0), p: h > 0.79 && h < 0.995 ? 1 : 0, gp: clamp01((h - 0.79) / 0.2) }),
      },
    },
    sdk: {
      ground: -0.25,
      crab: (h) => ({ pos: at(-0.6, 0, 0.15), pose2: 'work', pw: win(h, 0, 0.08) * (1 - win(h, 0.64, 0.72)) }),
      props: {
        disc: () => ({ pos: at(0.05, 0, 0), s: [1.75, 1, 1.5] }),
        ...Object.fromEntries(BLOCKS.map((b, i) => [`block${i}`, (h) => {
          const d = win(h, 0.04 + i * 0.13, 0.14 + i * 0.13);
          const gone = i >= 2 ? win(h, 0.66, 0.76) : 0;
          return { pos: at(0.62, i * BH + (1 - d) * 1.2, 0.1), ry: -0.12, p: d > 0.001 ? (1 - gone) : 0 };
        }])),
        file: (h) => ({ pos: at(0.62, 2 * BH, 0.2), ry: -0.12, p: win(h, 0.74, 0.84) }),
      },
    },
    workspaces: {
      ground: -1.8 * BS - 0.01,
      crab: () => ({ pos: at(0.62 * BS, 0, 0.6 * BS), pose2: 'work', pw: 1 }),
      props: {
        burrow: (h) => {
          const f1 = bump(h, 0.2, 0.42), f2 = bump(h, 0.6, 0.82);
          return { s: BS, flash: [0, f2 * 0.6, 0, f1] };
        },
        ghost0: (h) => ({ pos: at(-1.35 * BS, 0, 0.55 * BS), s: CS, ry: 0.3, p: bump(h, 0.04, 0.46) }),
        ghost1: (h) => ({ pos: at(-0.55 * BS, 0, -0.75 * BS), s: CS, ry: -0.2, p: bump(h, 0.44, 0.88) }),
      },
    },
    backends: {
      ground: -0.25,
      crab: (h) => ({ pos: at(0, 0, 0.6), ry: lerp(-0.55, 0.55, win(h, 0.1, 0.5)) * (1 - win(h, 0.7, 0.95) * 0.7) }),
      props: {
        disc: () => ({ s: [2.55, 1, 1.75] }),
        jar: () => ({ pos: at(-1.45, 0, -0.15), s: 0.62, lidOff: true }),
        greys: (h, c) => ({ pos: at(-1.45, 0.14 * 0.62, -0.15), clock: c }),
        sphere: (h, c) => ({ pos: at(1.45, 0, -0.15), clock: c }),
      },
    },
    tools: {
      ground: -0.25,
      crab: () => ({ pos: at(0, 0, 0), ry: -0.1, pose2: 'hold', pw: 1 }),
      props: {
        disc: () => ({ s: [1.55, 1, 1.55] }),
        belt: () => ({ pos: at(0, -0.06, 0) }),
        card: (h) => ({ h }),
      },
    },
    protocol: {
      ground: -0.25,
      crab: () => ({ pos: at(CRAB_PX, 0, 0.3), ry: -0.35, pose2: 'work', pw: 1 }),
      props: {
        disc: () => ({ pos: at(-0.2, 0, 0.1), s: [2.75, 1, 1.45] }),
        host: (h, c) => ({ pos: at(HOST_X, 0, -0.15), flash: Math.exp(-((((c * 0.28 + h * 0.8) % 1) + 1) % 1) * 12) }),
        cable: () => ({}),
        packets: (h, c) => ({ h, clock: c }),
      },
    },
    crabagent: {
      ground: -0.25,
      crab: (h) => ({ pos: at(-0.8, 0, 0.4), ry: 0.55, pose2: 'work', pw: 1 - win(h, 0.9, 1) }),
      props: {
        disc: () => ({ pos: at(0.15, 0, 0), s: [1.95, 1, 1.55] }),
        castle_base: (h) => ({ pos: at(CX, 0, -0.1), p: win(h, 0.02, 0.16) }),
        castle_towerL: (h) => ({ pos: at(CX - 0.62, 0, -0.1), p: win(h, 0.14, 0.3) }),
        castle_towerR: (h) => ({ pos: at(CX + 0.62, 0, -0.1), p: win(h, 0.22, 0.38) }),
        castle_cren: (h) => ({ pos: at(CX, 0.5, -0.1), p: win(h, 0.34, 0.5) }),
        castle_roofL: (h) => ({ pos: at(CX - 0.62, 0.95, -0.1), p: win(h, 0.46, 0.6) }),
        castle_roofR: (h) => ({ pos: at(CX + 0.62, 0.95, -0.1), p: win(h, 0.52, 0.66) }),
        castle_page: (h) => ({ pos: at(CX, 0.05, 0.37), p: win(h, 0.64, 0.8) }),
        castle_flag: (h, c) => ({ pos: at(CX + 0.62, 1.33, -0.1), p: win(h, 0.8, 0.94), clock: c }),
      },
    },
    start: {
      ground: -0.25,
      crab: (h) => ({ pos: at(0, 0, 0.35), pose2: 'wave', pw: win(h, 0, 0.12) }),
      props: {
        disc: () => ({ s: [2.5, 1, 2.0] }),
        water: (h, c) => ({ clock: c }),
        pebbles: () => ({}),
      },
    },
  };

  const stations = stationEls.map((el) => {
    const id = el.dataset.station;
    return { id, el, side: el.dataset.side || 'right', def: STATIONS[id], box: null, region: null };
  }).filter((s) => s.def);

  // ── theme ──
  function applyTheme() {
    const T = isDark() ? THEME.dark : THEME.light;
    catcherMat.opacity = T.shadow;
    hemi.intensity = T.hemi;
    sun.intensity = T.sun;
    mSand.color.set(T.sand);
    mDeep.color.set(T.sandDeep);
    waterMat.color.set(T.water);
    for (const g of glassMats) g.m.uniforms.uRim.value.set(T.rim);
    for (const m of rimLines) m.color.set(T.rim);
  }

  // ── pose blending for the claws (so 'idle' → 'wave' is a motion, not a cut) ──
  const clawKeys = [];
  for (const s of [1, -1]) {
    const c = crab.parts.claws[s];
    clawKeys.push([c.shoulder.rotation, 'x'], [c.shoulder.rotation, 'y'], [c.shoulder.rotation, 'z'],
      [c.wrist.rotation, 'x'], [c.wrist.rotation, 'y'], [c.jawTop.rotation, 'x'], [c.jawBot.rotation, 'x']);
  }
  const snap = new Float32Array(clawKeys.length);
  function poseCrab(a, b, w, time) {
    crab.setPose(a);
    crab.update(time, 0);
    if (!b || a === b || w <= 0.001) return;
    clawKeys.forEach(([o, key], i) => { snap[i] = o[key]; });
    crab.setPose(b);
    crab.update(time, 0);
    if (w >= 0.999) return;
    clawKeys.forEach(([o, key], i) => { o[key] = lerp(snap[i], o[key], w); });
  }
  function crabState(st, h, c) {
    const r = st.def.crab(h, c);
    return { pos: r.pos, ry: r.ry ?? 0, walk: r.walk ?? 0, pose2: r.pose2 ?? null, pw: r.pose2 ? (r.pw ?? 1) : 0, tint: r.tint ?? null };
  }

  // ── apply the film state: station a → station b at t (0..1), hold progress ha / hb ──
  function apply(a, b, t, ha, hb, clock, travel) {
    const e = ease(t);
    const A = crabState(a, ha, clock);
    const B = crabState(b, hb, clock);
    const pos = lerp3(A.pos, B.pos, e);
    crab.object3d.position.set(pos[0], pos[1], pos[2]);
    crab.object3d.rotation.y = lerp(A.ry, B.ry, e);
    crab.setWalk(Math.min(1, lerp(A.walk, B.walk, e) + Math.sin(Math.PI * t)));
    // claws: blend idle ⇄ pose. Two different poses blend through idle, so it never jumps.
    const wA = A.pose2 ? A.pw * (1 - e) : 0, wB = B.pose2 ? B.pw * e : 0;
    let p2 = null, w = 0;
    if (A.pose2 && A.pose2 === B.pose2) { p2 = A.pose2; w = wA + wB; }
    else if (wA >= wB) { p2 = A.pose2; w = wA - wB; }
    else { p2 = B.pose2; w = wB - wA; }
    poseCrab('idle', p2, Math.min(1, w), clock + travel);
    const tintA = A.tint, tintB = B.tint;
    if (tintA || tintB) {
      const tt = tintA && tintB ? lerp(tintA[1], tintB[1], e) : tintA ? tintA[1] * (1 - e) : tintB[1] * e;
      crab.setTint((e < 0.5 ? (tintA || tintB) : (tintB || tintA))[0], tt);
    } else crab.setTint(C.coral, 0);

    catcher.position.y = lerp(a.def.ground, b.def.ground, e);
    world.updateMatrixWorld(true);

    for (const [name, pr] of Object.entries(props)) {
      const fa = a.def.props[name], fb = b.def.props[name];
      const PA = fa ? { p: 1, ...fa(ha, clock) } : null;
      const PB = fb ? { p: 1, ...fb(hb, clock) } : null;
      let st = null;
      if (PA && PB) {
        st = { ...(e < 0.5 ? PA : PB) };
        st.pos = lerp3(PA.pos ?? [0, 0, 0], PB.pos ?? [0, 0, 0], e);
        const sa = vec(PA.s ?? 1), sb = vec(PB.s ?? 1);
        st.s = lerp3(sa, sb, e);
        st.ry = lerp(PA.ry ?? 0, PB.ry ?? 0, e);
        st.p = lerp(PA.p, PB.p, e);
        if (PA.lid !== undefined || PB.lid !== undefined) st.lid = lerp(PA.lid ?? 1, PB.lid ?? 1, e);
        // the jar comes back without its lid: the lid lifts away first
        if (PA.lidOff !== PB.lidOff) { st.lidOff = e > 0.5; }
      } else if (PA) st = { ...PA, p: PA.p * (1 - win(t, 0, 0.5)) }; // outgoing props are gone by mid-crossing
      else if (PB) st = { ...PB, p: PB.p * win(t, 0.5, 1) };      // incoming ones assemble after it
      setProp(pr, st);
    }
  }

  function setProp(pr, st) {
    const o = pr.obj;
    if (!st || st.p < 0.004) { o.visible = false; pr.after?.(null); return; }
    o.visible = true;
    const p = clamp01(st.p);
    const g = pr.mode === 'grow' ? Math.max(0.0001, backOut(p)) : 1;
    const s = vec(st.s ?? 1);
    o.scale.set(s[0] * g, s[1] * g, s[2] * g);
    const pos = st.pos ?? [0, 0, 0];
    const sink = pr.mode === 'fade' ? 0 : (1 - ease(p)) * pr.rise;
    if (!pr.local || st.pos) o.position.set(pos[0], pos[1] - sink, pos[2]);
    o.rotation.y = st.ry ?? 0;
    pr.after?.(st);
  }

  // ── measure each station's footprint in camera space (once) ──
  const _v = new THREE.Vector3(), _bb = new THREE.Box3();
  function measure(st) {
    let x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity;
    const samples = st.id === 'turn' ? [0, 0.3, 0.6, 1] : [0, 0.5, 1];
    for (const h of samples) {
      apply(st, st, 0, h, h, 1.2, 0);
      world.updateMatrixWorld(true);
      world.traverseVisible((o) => {
        if (!o.isMesh || o.userData.isOutline || o === grains) return;
        if (!o.geometry.boundingBox) o.geometry.computeBoundingBox();
        _bb.copy(o.geometry.boundingBox);
        for (let i = 0; i < 8; i++) {
          _v.set(i & 1 ? _bb.max.x : _bb.min.x, i & 2 ? _bb.max.y : _bb.min.y, i & 4 ? _bb.max.z : _bb.min.z).applyMatrix4(o.matrixWorld);
          if (_v.y < st.def.ground - 0.02) _v.y = st.def.ground; // ignore things sunk into the sand
          _v.applyMatrix4(camera.matrixWorldInverse);
          x0 = Math.min(x0, _v.x); x1 = Math.max(x1, _v.x); y0 = Math.min(y0, _v.y); y1 = Math.max(y1, _v.y);
        }
      });
    }
    // headroom for idle motion (lid lift, claw wave, card flip)
    const padX = (x1 - x0) * 0.04, padY = (y1 - y0) * 0.06;
    return { cx: (x0 + x1) / 2, cy: (y0 + y1) / 2, w: x1 - x0 + 2 * padX, h: y1 - y0 + 2 * padY };
  }
  for (const st of stations) st.box = measure(st);

  // ── layout: screen regions + scroll keyframes ──
  const mqMobile = matchMedia('(max-width: 860px)');
  const reduce = matchMedia('(prefers-reduced-motion: reduce)');
  let W = 1, H = 1, mobile = false;
  let keys = []; // per station: { holdStart, holdEnd }
  function layout() {
    mobile = mqMobile.matches;
    W = Math.max(1, host.clientWidth);
    H = Math.max(1, host.clientHeight);
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, mobile ? 1.5 : 2));
    renderer.setSize(W, H, false);
    document.body.classList.toggle('film-on', true);
    const vw = window.innerWidth, vh = window.innerHeight;
    const sy = window.scrollY;
    const barH = document.querySelector('.bar')?.getBoundingClientRect().height ?? 72;
    stations.forEach((st, i) => {
      let r;
      if (mobile) {
        const drift = st.side === 'right' ? 0.12 : -0.12;
        r = { cx: W * (0.5 + drift), cy: H * 0.58, w: W * 0.6, h: H * 0.8 };
      } else {
        const a = st.el.querySelector('.aside').getBoundingClientRect();
        let L, Rr;
        if (st.side === 'right') { L = a.left + 8; Rr = Math.min(vw - 48, a.right + 150); } else { L = Math.max(48, a.left - 150); Rr = a.right - 8; }
        let top = barH + 28, bottom = vh - 28;
        if (i === 0) top = Math.max(top, a.top + sy + 8); // hero: below the masthead at scroll 0
        r = { cx: (L + Rr) / 2, cy: (top + bottom) / 2, w: Rr - L, h: bottom - top };
      }
      const maxPpu = mobile ? H * 0.42 : Math.min(200, vh * 0.23);
      r.ppu = Math.min(r.w * 0.9 / st.box.w, r.h * 0.82 / st.box.h, maxPpu);
      st.region = r;
    });
    // keyframes on the scroll axis
    const docTop = (el) => el.getBoundingClientRect().top + sy;
    const maxScroll = Math.max(1, document.documentElement.scrollHeight - vh);
    keys = stations.map(() => ({ holdStart: 0, holdEnd: maxScroll }));
    let prevEnd = 0;
    for (let i = 0; i < stations.length - 1; i++) {
      const next = stations[i + 1].el;
      const B = docTop(next);
      const pad = parseFloat(getComputedStyle(next).paddingTop) || 0;
      let mid, half;
      if (mobile) { mid = B - vh * 0.5; half = vh * 0.2; } else {
        const cyMid = (stations[i].region.cy + stations[i + 1].region.cy) / 2;
        mid = B + pad / 2 - cyMid; half = vh * 0.3;
      }
      const s0 = Math.max(prevEnd, mid - half);
      const s1 = Math.max(s0 + 1, mid + half);
      keys[i].holdEnd = s0;
      keys[i + 1].holdStart = s1;
      prevEnd = s1;
    }
    keys[stations.length - 1].holdEnd = Math.max(keys[stations.length - 1].holdStart + 1, maxScroll);
  }

  /** scroll → { a, b, t, ha, hb, travel } */
  function locate(y) {
    for (let i = 0; i < stations.length; i++) {
      const kk = keys[i];
      if (y <= kk.holdEnd || i === stations.length - 1) {
        if (y < kk.holdStart && i > 0) {
          const kp = keys[i - 1];
          const t = clamp01((y - kp.holdEnd) / (kk.holdStart - kp.holdEnd));
          return { a: i - 1, b: i, t, ha: 1, hb: 0 };
        }
        const h = clamp01((y - kk.holdStart) / Math.max(1, kk.holdEnd - kk.holdStart));
        return { a: i, b: i, t: 0, ha: h, hb: h };
      }
    }
    return { a: 0, b: 0, t: 0, ha: 0, hb: 0 };
  }

  // ── render ──
  let clock = 0;
  function render() {
    const y = window.scrollY;
    let { a, b, t, ha, hb } = locate(y);
    const still = reduce.matches;
    if (still) {
      // reduced motion: snap to the nearest station's static pose; no scrub, no idle loop
      const i = t < 0.5 ? a : b;
      a = b = i; t = 0;
      ha = hb = REST[stations[i].id] ?? 0.5;
    }
    const sa = stations[a], sb = stations[b];
    const travel = still ? 0 : (a + t) * 2.0 + y * 0.0015;
    const c = still ? 1.2 : clock;
    apply(sa, sb, t, ha, hb, c, travel);

    // camera: shift + zoom the ortho frustum so the station box lands in its region
    const e = ease(t);
    const ra = sa.region, rb = sb.region;
    const ax = lerp(ra.cx, rb.cx, e), ay = lerp(ra.cy, rb.cy, e);
    const dip = mobile ? 1 - 0.15 * Math.sin(Math.PI * t) : 1 - 0.4 * Math.sin(Math.PI * t);
    const ppu = Math.exp(lerp(Math.log(ra.ppu), Math.log(rb.ppu), e)) * dip;
    const cx = lerp(sa.box.cx, sb.box.cx, e), cy = lerp(sa.box.cy, sb.box.cy, e);
    camera.left = cx - ax / ppu; camera.right = cx + (W - ax) / ppu;
    camera.top = cy + ay / ppu; camera.bottom = cy - (H - ay) / ppu;
    camera.updateProjectionMatrix();
    setOutlineWidth(mobile ? 1.2 : 1.6);
    renderer.render(scene, camera);
  }

  // ── loop: on-demand while scrolling, ~30 fps idle, paused when hidden ──
  let raf = 0, lastScroll = -1e9, lastFrame = 0, prev = performance.now();
  function tick(now) {
    raf = requestAnimationFrame(tick);
    const dt = Math.min(0.05, Math.max(0, (now - prev) / 1000));
    prev = now;
    clock += dt;
    const scrolling = now - lastScroll < 300;
    if (!scrolling && now - lastFrame < 1000 / 30 - 2) return;
    lastFrame = now;
    render();
  }
  function sync() {
    const run = !document.hidden && !reduce.matches;
    if (run && !raf) { prev = performance.now(); raf = requestAnimationFrame(tick); }
    if (!run && raf) { cancelAnimationFrame(raf); raf = 0; }
    if (!run) render();
  }
  window.addEventListener('scroll', () => { lastScroll = performance.now(); if (!raf) render(); }, { passive: true });
  let relayout = 0;
  const onResize = () => { cancelAnimationFrame(relayout); relayout = requestAnimationFrame(() => { layout(); render(); }); };
  window.addEventListener('resize', onResize);
  new ResizeObserver(onResize).observe(document.querySelector('main'));
  mqMobile.addEventListener('change', onResize);
  document.fonts?.ready.then(onResize);
  document.addEventListener('visibilitychange', sync);
  reduce.addEventListener('change', sync);
  const onTheme = () => { applyTheme(); render(); };
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', onTheme);
  new MutationObserver(onTheme).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

  applyTheme();
  layout();
  render();
  sync();
  host.classList.add('live');
  const api = {
    render, layout,
    info: () => ({ calls: renderer.info.render.calls, triangles: renderer.info.render.triangles, keys, stations: stations.map((s) => ({ id: s.id, box: s.box, region: s.region })), at: locate(window.scrollY) }),
  };
  host.sandcrabFilm = api; // for debugging from the console
  return api;
}
