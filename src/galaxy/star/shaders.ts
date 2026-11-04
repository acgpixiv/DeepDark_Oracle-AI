// 星图 3D 渲染用到的贴图与着色器（《2D转3D与星图需求明细》#12/#13/#14）。
//
// 为什么贴图要自己用 Canvas 画：3000 个粒子 + 上万个发光节点如果各带一张
// PNG，安装包会凭空涨几 MB，而且加载是异步的会闪一下。512×512 的径向渐变
// 在启动时算一次只要几毫秒，还能随手调色。
import * as THREE from "three";

/** 发光球体贴图：中心纯白、边缘完全透明（需求 #13）。 */
export function glowTexture(size = 512): THREE.Texture {
  const c = document.createElement("canvas");
  c.width = c.height = size;
  const g = c.getContext("2d")!;
  const r = size / 2;
  const grad = g.createRadialGradient(r, r, 0, r, r, r);
  grad.addColorStop(0, "rgba(255,255,255,1)");
  grad.addColorStop(0.14, "rgba(255,255,255,0.95)");
  grad.addColorStop(0.32, "rgba(255,255,255,0.42)");
  grad.addColorStop(0.58, "rgba(255,255,255,0.12)");
  grad.addColorStop(0.82, "rgba(255,255,255,0.03)");
  grad.addColorStop(1, "rgba(255,255,255,0)");
  g.fillStyle = grad;
  g.fillRect(0, 0, size, size);
  const t = new THREE.CanvasTexture(c);
  t.colorSpace = THREE.SRGBColorSpace;
  t.needsUpdate = true;
  return t;
}

/** 粒子贴图：比 glow 更小的硬芯，避免 3000 个粒子叠成棉花糖。 */
export function dotTexture(size = 64): THREE.Texture {
  const c = document.createElement("canvas");
  c.width = c.height = size;
  const g = c.getContext("2d")!;
  const r = size / 2;
  const grad = g.createRadialGradient(r, r, 0, r, r, r);
  grad.addColorStop(0, "rgba(255,255,255,1)");
  grad.addColorStop(0.35, "rgba(255,255,255,0.75)");
  grad.addColorStop(1, "rgba(255,255,255,0)");
  g.fillStyle = grad;
  g.fillRect(0, 0, size, size);
  const t = new THREE.CanvasTexture(c);
  t.colorSpace = THREE.SRGBColorSpace;
  t.needsUpdate = true;
  return t;
}

/**
 * 星尘云团贴图（需求 #11 的"星云"质感）：低分辨率的大块柔和噪声，
 * 直接乘在深空底色上。用 256×256 + 强模糊生成，比在 shader 里跑 fbm 便宜。
 */
export function cloudTexture(size = 256): THREE.Texture {
  const c = document.createElement("canvas");
  c.width = c.height = size;
  const g = c.getContext("2d")!;
  g.fillStyle = "rgba(0,0,0,0)";
  g.fillRect(0, 0, size, size);
  // 随机摆放若干柔和色斑，再让 Canvas 的 globalAlpha 叠加自然形成层次。
  let seed = 0x9e3779b9;
  const rnd = () => {
    seed ^= seed << 13;
    seed ^= seed >>> 17;
    seed ^= seed << 5;
    return ((seed >>> 0) % 100000) / 100000;
  };
  // 青主调 + 蓝紫 + 橙点缀（与星尘调色一致，需求 #11）。
  const colors = ["96,220,255", "150,130,255", "255,180,90"];
  // 26 个斑几乎把整张图铺满，乘到球壳上就是一整片均匀的灰。减到 16 个、
  // 半径收到 0.10~0.30，天空才会留出大块的真空黑。
  for (let i = 0; i < 16; i++) {
    const cx = rnd() * size;
    const cy = rnd() * size;
    const r = size * (0.1 + rnd() * 0.2);
    const grad = g.createRadialGradient(cx, cy, 0, cx, cy, r);
    const rgb = colors[Math.floor(rnd() * colors.length)] ?? colors[0];
    // 这个球壳现在是 NormalBlending + opacity 0.55：贴图 alpha 是"这一块
    // 云有多实"，不再是加色亮度。原来 0.05~0.15 是给叠加式当亮度用的，
    // 换成普通混合后天空会整片发灰（实测正文区中位亮度 35，深空底才 5）；
    // 压到 0.012~0.038 后只剩几缕极淡的雾，角落仍能读出 #050510 的黑。
    const a = 0.012 + rnd() * 0.026;
    grad.addColorStop(0, `rgba(${rgb},${a.toFixed(3)})`);
    grad.addColorStop(1, `rgba(${rgb},0)`);
    g.fillStyle = grad;
    g.beginPath();
    g.arc(cx, cy, r, 0, Math.PI * 2);
    g.fill();
  }
  const t = new THREE.CanvasTexture(c);
  t.colorSpace = THREE.SRGBColorSpace;
  t.wrapS = t.wrapT = THREE.ClampToEdgeWrapping;
  t.needsUpdate = true;
  return t;
}

/**
 * 节点着色器（#12/#14）：把每个节点画成一个永远面向相机的发光圆盘。
 *
 * 不用 Sprite 是因为 3d-force-graph 里上万个 Sprite 会变成上万个 draw call；
 * 这里所有节点共用一份 InstancedBufferGeometry，只有 1 个 draw call（#29）。
 */
/**
 * 恒星专用着色器（需求 #1「发光球体 + 日冕特效」）。
 *
 * 为什么不能沿用节点着色器：
 *   1. 节点是"扁平发光圆盘"，恒星必须是**有体积的球**——中心高亮、边缘
 *      按球面法线压暗，才有"温度梯度"的观感；
 *   2. 恒星要有**日冕**（外圈几倍大的柔光）和**表面耀斑**（噪声起伏），
 *      这两样都要在片元里按半径分段处理，圆盘着色器没有这个结构；
 *   3. 恒星数量少（几个盘符），可以单独一次 draw call 用更贵的着色，
 *      不能让它拖累上万个性节点。
 *
 * 所有参数（颜色 / 亮度 / 大小 / 耀斑强度）都由实例属性给，CPU 侧改一次
 * 属性就生效，不重建材质。
 */
export const STAR_VERT = /* glsl */ `
attribute vec3 aOffset;
attribute vec3 aColor;
attribute vec3 aCorona;
attribute float aSize;
attribute float aAlpha;
attribute float aFlare;
attribute float aSeed;
varying vec2 vUv;
varying vec3 vColor;
varying vec3 vCorona;
varying float vAlpha;
varying float vFlare;
varying float vSeed;
varying float vDist;
void main() {
  vUv = uv;
  vColor = aColor;
  vCorona = aCorona;
  vAlpha = aAlpha;
  vFlare = aFlare;
  vSeed = aSeed;
  vec4 mv = modelViewMatrix * vec4(aOffset, 1.0);
  vDist = -mv.z;
  // 恒星用比普通节点更大的四边形：日冕要铺到核心半径的 2.6 倍左右。
  //
  // 硬上限 420：恒星尺寸是"容量 → 半径"算出来的，一旦公式或单位改错
  // （比如把 TB 当成 GB），billboard 会大到盖住整个视口，屏幕上只剩
  // 一块纯色矩形 —— 2026-11-04 实测踩过。这一行让着色器自己兜底。
  float starSize = min(aSize * 2.6, 420.0);
  mv.xy += position.xy * starSize;
  gl_Position = projectionMatrix * mv;
}
`;

export const STAR_FRAG = /* glsl */ `
uniform float uTime;
varying vec2 vUv;
varying vec3 vColor;
varying vec3 vCorona;
varying float vAlpha;
varying float vFlare;
varying float vSeed;
varying float vDist;

// 廉价的 2D 噪声：恒星只有个位数，跑 fbm 也不会影响帧率，
// 但改成三次哈希叠加能让同一份代码在低端机上也不掉帧。
float hash(vec2 p) {
  return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453123);
}
float noise(vec2 p) {
  vec2 i = floor(p);
  vec2 f = fract(p);
  vec2 u = f * f * (3.0 - 2.0 * f);
  return mix(mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x),
             mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x), u.y);
}

void main() {
  // 以四边形中心为原点，d = 0 是恒星中心，d = 1 是四边形边缘。
  vec2 p = vUv * 2.0 - 1.0;
  float d = length(p);
  if (d > 1.0) discard;

  // ---- 核心球体：半径 0~0.42，中心纯白 → 边缘本色 --------------------
  // 用 1 - d/0.42 的四次方做衰减，比线性更接近黑体辐射的亮度曲线。
  float coreR = 0.42;
  float core = 0.0;
  if (d < coreR) {
    float t = d / coreR;
    // 1.25 次方：中心仍是最亮，但高亮区域收得更紧，
    // 1.6 次方会让 40% 半径内全部接近满亮度，叠起来就是一颗白球。
    core = pow(1.0 - t * t, 1.25) * 0.82;
  }

  // ---- 表面耀斑：随时间的噪声扰动（需求 #6「高 IO 时耀斑爆发」）------
  // flare 由 CPU 侧按磁盘活跃度给（0 = 平静，1 = 爆发）。
  float ang = atan(p.y, p.x);
  float n = noise(vec2(ang * 2.2 + vSeed * 7.0 + uTime * 0.35,
                       d * 3.4 - uTime * 0.28));
  float granule = 1.0 + (n - 0.5) * (0.32 + vFlare * 0.85);
  core *= granule;

  // ---- 日冕：0.42~1.0 的柔光，指数衰减 -------------------------------
  // 关键：日冕的颜色偏暖（corona），和核心的白形成温度层次。
  float coronaT = clamp((d - coreR) / (1.0 - coreR), 0.0, 1.0);
  float corona = pow(1.0 - coronaT, 3.2) * 0.55;
  // 加一点脉动，让恒星"活着"但幅度很小（超过 8% 会显得闪）。
  corona *= 0.94 + 0.06 * sin(uTime * 1.3 + vSeed * 6.2831);

  // ---- 合成 ---------------------------------------------------------
  // 核心本色偏白但**不要纯白**：mix 到 0.35 时核心仍保留恒星的光谱色，
  // 0.55 会让 1TB 以上的盘整颗烧成白球 —— 实测 D/E 两颗都是纯白心，
  // 看不出"系统盘蓝白 / 仓库盘橙红"的分级，那这条需求就白做了。
  vec3 coreCol = mix(vColor, vec3(1.0), 0.35) * core;
  // 日冕压到 0.34：0.55 在 Bloom 打开时会连成一片，两颗恒星之间糊成一大团。
  vec3 ringCol = vCorona * corona;
  vec3 col = coreCol + ringCol;

  // 总亮度上限：加色混合下 >1 的部分会被 Bloom 再放大一圈，
  // 深空背景 + 多颗恒星叠起来就会糊成白团（v0.26 踩过这个坑）。
  float a = clamp(core + corona, 0.0, 1.0) * vAlpha;
  if (a < 0.002) discard;
  gl_FragColor = vec4(col * a, a);
}
`;

export const NODE_VERT = /* glsl */ `
attribute vec3 aOffset;
attribute vec3 aColor;
attribute float aSize;
attribute float aAlpha;
// LOD 系数（#30）：1 = 近景完整光晕，越小越像远处的"简略亮点"。
attribute float aLod;
// 自转相位（需求 #5/#6）：每颗天体转多快、从哪个角度起转。
attribute float aSpin;
uniform float uLodScale;
varying vec2 vUv;
varying vec3 vColor;
varying float vAlpha;
varying float vLod;
varying float vSpin;
void main() {
  vUv = uv;
  vColor = aColor;
  vAlpha = aAlpha;
  vLod = aLod;
  vSpin = aSpin;
  // billboard：把四边形放到实例位置，并在视空间里按像素尺寸展开，
  // 这样节点永远正对相机，缩放时也不会变成侧过来的薄片。
  // LOD 只缩尺寸不缩光晕范围，避免远处节点彻底消失（#30 要求平滑过渡）。
  vec4 mv = modelViewMatrix * vec4(aOffset, 1.0);
  mv.xy += position.xy * aSize * mix(0.55, 1.0, aLod) * uLodScale;
  gl_Position = projectionMatrix * mv;
}
`;

export const NODE_FRAG = /* glsl */ `
uniform sampler2D uGlow;
uniform float uTime;
varying vec2 vUv;
varying vec3 vColor;
varying float vAlpha;
varying float vLod;
varying float vSpin;
void main() {
  // 自转（需求 #5/#6）：光晕本来是径向对称的，转起来看不出任何变化。
  // 这里按极角叠一层缓慢行进的明暗条纹，天体就有了"表面在转"的观感；
  // 条纹只调制亮度、不改颜色，所以不会破坏现有的发光质感。
  vec2 d = vUv * 2.0 - 1.0;
  float ang = atan(d.y, d.x);
  float spin = 0.86 + 0.14 * sin(ang * 5.0 + uTime * 1.7 * (0.6 + vSpin) + vSpin * 6.2831);
  vec4 t = texture2D(uGlow, vUv);
  if (t.a < 0.004) discard;
  // 远处节点（vLod→0）把外圈光晕收掉，只留亮芯：等价于低模 Sprite（#30），
  // 既省填充率，也让近处的发光层次更突出。
  float core = smoothstep(0.62, 1.0, t.a);
  // 亮芯峰值 0.95（不是 1.35）：加色混合下 >1 的部分只会被帧缓冲钳掉，
  // 却会在 Bloom 里被当成"超亮区域"再放大一圈，近处节点一多就糊成白团。
  float a = mix(core * 0.95, t.a, vLod) * spin;
  // AdditiveBlending 下颜色要预乘 alpha，否则边缘会发灰。
  gl_FragColor = vec4(vColor * a, a * vAlpha);
}
`;

/** 星尘粒子（#11）：中心亮、边缘散，带一点随距离的呼吸感。 */
export const PARTICLE_VERT = /* glsl */ `
attribute vec3 aColor;
attribute float aSize;
attribute float aPhase;
uniform float uTime;
uniform float uPixel;
varying vec3 vColor;
varying float vAlpha;
void main() {
  vColor = aColor;
  vec4 mv = modelViewMatrix * vec4(position, 1.0);
  float twinkle = 0.72 + 0.28 * sin(uTime * 1.6 + aPhase * 6.2831);
  vAlpha = twinkle;
  gl_PointSize = aSize * uPixel * (300.0 / max(-mv.z, 1.0));
  gl_Position = projectionMatrix * mv;
}
`;

export const PARTICLE_FRAG = /* glsl */ `
uniform sampler2D uDot;
varying vec3 vColor;
varying float vAlpha;
void main() {
  vec4 t = texture2D(uDot, gl_PointCoord);
  if (t.a < 0.01) discard;
  gl_FragColor = vec4(vColor * t.a, t.a * vAlpha);
}
`;