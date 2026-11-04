// 深空场景：星尘 + 星雾 + 自转（《2D转3D与星图需求明细》#8/#9/#11）。
//
// 粒子用单个 BufferGeometry + 一次 draw call —— 3000 个 Sprite 在集成显卡上
// 直接把帧率打到个位数，而 3000 个点云只占 3000×3 个 float。

import * as THREE from "three";
import { cloudTexture, dotTexture, PARTICLE_FRAG, PARTICLE_VERT } from "./shaders";

export interface NebulaOptions {
  /** 粒子数量（需求 #11：默认 3000）。 */
  count: number;
  /** 半径；粒子按 cbrt 分布填充球体，避免中心过密。 */
  radius: number;
  /** 场景雾密度（需求 #8：0.002）。 */
  fogDensity: number;
  /** 自转速度（弧度/秒，需求 #8：0.5 度级别）。 */
  spin: number;
}

export const NEBULA_DEFAULTS: NebulaOptions = {
  count: 3000,
  radius: 900,
  fogDensity: 0.002,
  spin: 0.08,
};

/** 主色青、点缀橙（需求 #11：#00ffff / #ffaa00）。 */
const PALETTE: Array<[number, [number, number, number]]> = [
  [0.62, [0.0, 1.0, 1.0]],
  [0.18, [1.0, 0.667, 0.0]],
  [0.14, [0.62, 0.55, 1.0]],
  [0.06, [0.85, 0.95, 1.0]],
];

function pickColor(rnd: number): [number, number, number] {
  let acc = 0;
  for (const [w, c] of PALETTE) {
    acc += w;
    if (rnd <= acc) return c;
  }
  return PALETTE[0][1];
}

export class StarNebula {
  readonly group = new THREE.Group();
  private points: THREE.Points;
  private geom: THREE.BufferGeometry;
  private mat: THREE.ShaderMaterial;
  /** 粒子间连线（需求 #11）：1 次 draw call 的 LineSegments。 */
  private linkLines: THREE.LineSegments;
  private linkGeom: THREE.BufferGeometry;
  private linkMat: THREE.LineBasicMaterial;
  /** 星云内壁（背景云团）。 */
  private cloud: THREE.Mesh;
  private cloudGeom: THREE.BufferGeometry;
  private cloudMat: THREE.MeshBasicMaterial;
  private opts: NebulaOptions;
  private time = 0;
  private disposed = false;

  constructor(opts: Partial<NebulaOptions> = {}) {
    this.opts = { ...NEBULA_DEFAULTS, ...opts };
    const n = Math.max(0, Math.floor(this.opts.count));
    const pos = new Float32Array(n * 3);
    const col = new Float32Array(n * 3);
    const size = new Float32Array(n);
    const phase = new Float32Array(n);

    // 简单可复现的伪随机：每次打开星图星尘分布一致，切换视图不会"闪一下换天"。
    let seed = 0x2f6e2b1;
    const rnd = () => {
      seed ^= seed << 13;
      seed ^= seed >>> 17;
      seed ^= seed << 5;
      return ((seed >>> 0) % 100000) / 100000;
    };

    for (let i = 0; i < n; i++) {
      // 球内均匀分布用 cbrt(r)：直接 r*random 会把粒子堆在中心。
      const r = this.opts.radius * Math.cbrt(rnd());
      const theta = rnd() * Math.PI * 2;
      const phi = Math.acos(2 * rnd() - 1);
      const sx = Math.sin(phi) * Math.cos(theta);
      const sy = Math.sin(phi) * Math.sin(theta);
      const sz = Math.cos(phi);
      pos[i * 3] = sx * r;
      pos[i * 3 + 1] = sy * r;
      pos[i * 3 + 2] = sz * r;
      const [cr, cg, cb] = pickColor(rnd());
      col[i * 3] = cr;
      col[i * 3 + 1] = cg;
      col[i * 3 + 2] = cb;
      size[i] = 1.1 + rnd() * 2.6;
      phase[i] = rnd();
    }

    this.geom = new THREE.BufferGeometry();
    this.geom.setAttribute("position", new THREE.BufferAttribute(pos, 3));
    this.geom.setAttribute("aColor", new THREE.BufferAttribute(col, 3));
    this.geom.setAttribute("aSize", new THREE.BufferAttribute(size, 1));
    this.geom.setAttribute("aPhase", new THREE.BufferAttribute(phase, 1));
    // 点云永远在视锥内（包住整个场景），关掉包围盒计算省一次遍历。
    this.geom.boundingSphere = new THREE.Sphere(new THREE.Vector3(), this.opts.radius * 1.2);

    this.mat = new THREE.ShaderMaterial({
      uniforms: {
        uTime: { value: 0 },
        uDot: { value: dotTexture() },
        uPixel: { value: Math.min(2, window.devicePixelRatio || 1) },
      },
      vertexShader: PARTICLE_VERT,
      fragmentShader: PARTICLE_FRAG,
      transparent: true,
      depthWrite: false,
      blending: THREE.AdditiveBlending,
      fog: false,
    });

    this.points = new THREE.Points(this.geom, this.mat);
    this.points.frustumCulled = false;
    this.points.name = "star-nebula";
    this.group.add(this.points);

    // ---- 粒子间连线（需求 #11）：淡蓝紫的稀疏星尘丝 --------------------
    // 粒子是按 cbrt 均匀撒在球体内部的，**编号之间没有任何空间关系**。
    // 早先按"编号相邻"配对，连出来的是穿越球心的长线；后来改成"随机取 8 个
    // 候选挑最近"，长度阈值仍是 radius*0.22（≈198），700 条这么长的线在画面里
    // 依旧交叉成一张横贯全屏的网 —— 因为阈值本身就是"跨屏"尺度。
    //
    // 需求 #11 的原话是「仅连接距离小于阈值的粒子对（如 dist < 15）、淡色、
    // 不抢视觉重心」，那个 15 是针对"几千个粒子挤在半径百来单位的小场景"说的。
    // 本项目星尘半径 900，按同比例归一化后阈值只有 radius*0.07（≈63）——
    // 比原来的 198 短三倍，投影出来才是"短絮"而不是"跨屏直线"。
    //
    // 光有阈值不够：随机撒点下，半径 63 的球内平均期望邻居只有 1.03 个，
    // 用随机采样找邻居命中率仅 0.4%，凑不出几条线。所以按需求建议上空间加速
    // 结构 —— 这里用 O(n) 的空间哈希网格（KD-Tree 对本场景是杀鸡用牛刀），
    // 只在 3×3×3 邻格内找最近邻，每个粒子最多参与一条线段。
    const segLimit = Math.min(220, Math.floor(n * 0.09));
    const lp = new Float32Array(segLimit * 6);
    const maxLen = this.opts.radius * 0.07;
    const maxLen2 = maxLen * maxLen;
    let segs = 0;
    if (segLimit > 0 && n > 1) {
      // 空间哈希：格子边长取阈值，于是"距离 < 阈值"必然落在 27 邻格中。
      const cell = maxLen;
      const grid = new Map<number, number[]>();
      const cellKey = (x: number, y: number, z: number) => {
        // 坐标范围固定在 ±radius*1.2，量化成整数后打包进一个 number 做键，
        // 比拼字符串快一个数量级（每帧不跑，只在构建时跑一遍）。
        const bx = Math.floor(x / cell) + 64;
        const by = Math.floor(y / cell) + 64;
        const bz = Math.floor(z / cell) + 64;
        return (bx * 128 + by) * 128 + bz;
      };
      for (let i = 0; i < n; i++) {
        const k = cellKey(pos[i * 3], pos[i * 3 + 1], pos[i * 3 + 2]);
        const bucket = grid.get(k);
        if (bucket) bucket.push(i);
        else grid.set(k, [i]);
      }
      const used = new Uint8Array(n);
      for (let i = 0; i < n && segs < segLimit; i++) {
        if (used[i]) continue;
        const x0 = pos[i * 3];
        const y0 = pos[i * 3 + 1];
        const z0 = pos[i * 3 + 2];
        let best = -1;
        let bestD = maxLen2;
        for (let ox = -1; ox <= 1; ox++) {
          for (let oy = -1; oy <= 1; oy++) {
            for (let oz = -1; oz <= 1; oz++) {
              const bucket = grid.get(cellKey(x0 + ox * cell, y0 + oy * cell, z0 + oz * cell));
              if (!bucket) continue;
              for (let bi = 0; bi < bucket.length; bi++) {
                const c = bucket[bi];
                if (c === i || used[c]) continue;
                const dx = x0 - pos[c * 3];
                const dy = y0 - pos[c * 3 + 1];
                const dz = z0 - pos[c * 3 + 2];
                const d = dx * dx + dy * dy + dz * dz;
                if (d < bestD) {
                  bestD = d;
                  best = c;
                }
              }
            }
          }
        }
        if (best < 0) continue; // 附近没邻居就不连，宁可空着也不画长线
        used[i] = 1;
        used[best] = 1;
        lp[segs * 6] = x0;
        lp[segs * 6 + 1] = y0;
        lp[segs * 6 + 2] = z0;
        lp[segs * 6 + 3] = pos[best * 3];
        lp[segs * 6 + 4] = pos[best * 3 + 1];
        lp[segs * 6 + 5] = pos[best * 3 + 2];
        segs++;
      }
    }
    this.linkGeom = new THREE.BufferGeometry();
    this.linkGeom.setAttribute("position", new THREE.BufferAttribute(lp.slice(0, segs * 6), 3));
    this.linkMat = new THREE.LineBasicMaterial({
      color: 0xaab8ff,
      transparent: true,
      // 0.15 → 0.11 → 0.07 → 0.06：粒子线是加色的，短线两端也会叠在一起。
      // 长度和数量都砍到原来的 1/3 之后，这一档才是"看得见但不抢戏"。
      opacity: 0.06,
      blending: THREE.AdditiveBlending,
      depthWrite: false,
      // 开雾衰减：远处的星尘丝随 FogExp2 融进深空底，不再是一层均匀的白纱
      // （没有 scene.fog 时 three 会自动跳过，关掉深空模式也不出错）。
      fog: true,
    });
    this.linkLines = new THREE.LineSegments(this.linkGeom, this.linkMat);
    this.linkLines.frustumCulled = false;
    this.linkLines.name = "star-nebula-links";
    this.group.add(this.linkLines);

    // ---- 星云内壁（需求 #17 赛博朋克数据星云）--------------------------
    // 相机永远在这个球壳内部，所以用 BackSide 的球体内壁：不用追踪相机、
    // 也不用担心贴图边缘穿帮，1 次 draw call 就能铺满整个天空。
    this.cloudGeom = new THREE.SphereGeometry(this.opts.radius * 1.6, 24, 16);
    this.cloudMat = new THREE.MeshBasicMaterial({
      map: cloudTexture(),
      side: THREE.BackSide,
      transparent: true,
      // 0.9 是"叠加式"时代的数值：AdditiveBlending 下单块贴图叠 26 个斑，
      // 再来一层 Bloom，整屏直接饱和成白色。换成普通 alpha 混合后 0.55 仍然
      // 偏亮（实测正文区中位亮度 35，深空底只有 5），再压到 0.34，
      // 这层只负责给深空底"铺几缕极淡的雾"，不再自作主张当光源。
      opacity: 0.34,
      depthWrite: false,
      blending: THREE.NormalBlending,
      fog: false,
    });
    this.cloud = new THREE.Mesh(this.cloudGeom, this.cloudMat);
    this.cloud.frustumCulled = false;
    this.cloud.name = "star-nebula-cloud";
    this.group.add(this.cloud);
  }

  setPixelRatio(dpr: number) {
    this.mat.uniforms.uPixel.value = Math.max(0.5, Math.min(3, dpr));
  }

  /** 各向异性过滤（#31）：星云内壁是大球面，斜着看最容易糊。 */
  setAnisotropy(v: number) {
    const aniso = Math.max(1, Math.min(16, v || 1));
    const dot = this.mat.uniforms.uDot?.value as THREE.Texture | undefined;
    if (dot) dot.anisotropy = aniso;
    if (this.cloudMat.map) {
      this.cloudMat.map.anisotropy = aniso;
      this.cloudMat.map.generateMipmaps = true;
      this.cloudMat.map.needsUpdate = true;
    }
  }

  /** 每帧推进自转与闪烁；返回 false 表示不用再画（组件已卸载）。 */
  update(dt: number) {
    if (this.disposed) return;
    this.time += dt;
    this.mat.uniforms.uTime.value = this.time;
    this.group.rotation.y += this.opts.spin * dt;
    this.group.rotation.x = Math.sin(this.time * 0.05) * 0.06;
  }

  dispose() {
    this.disposed = true;
    this.group.remove(this.points);
    this.group.remove(this.linkLines);
    this.group.remove(this.cloud);
    this.geom.dispose();
    this.linkGeom.dispose();
    this.linkMat.dispose();
    this.cloudGeom.dispose();
    this.cloudMat.map?.dispose();
    this.cloudMat.dispose();
    const tex = this.mat.uniforms.uDot?.value as THREE.Texture | undefined;
    tex?.dispose();
    this.mat.dispose();
  }
}

/** 给场景套上深空底色与指数雾（需求 #8）。 */
export function applyDeepSpace(scene: THREE.Scene, density = NEBULA_DEFAULTS.fogDensity) {
  scene.background = new THREE.Color(0x050510);
  // FogExp2 而不是线性雾：线性雾的远端会把远处节点整块抹平，
  // 指数雾能保留"越远越暗但仍看得见"的层次感。
  scene.fog = new THREE.FogExp2(0x050510, density);
}

export function clearDeepSpace(scene: THREE.Scene, background = 0x080b14) {
  scene.background = new THREE.Color(background);
  scene.fog = null;
}
