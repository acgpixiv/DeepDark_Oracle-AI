// 3D 星图引擎（《2D转3D与星图需求明细》#1~#18、#29~#32）。
//
// 为什么不用 3d-force-graph 自带的节点渲染：
// 它给每个节点建一个 SphereGeometry Mesh，一万节点就是一万次 draw call，
// 直接违反需求 #29（10000 节点 < 10 draw call）。这里让 ForceGraph3D 只负责
// **力学模拟与相机**（nodeVisibility 全关），节点的绘制交给一份
// InstancedBufferGeometry —— 全场景节点 1 次 draw call、连线 1 次、星尘 1 次。

import ForceGraph3D from "3d-force-graph";
import * as THREE from "three";
import { UnrealBloomPass } from "three/examples/jsm/postprocessing/UnrealBloomPass.js";
import type { StarEdgeOut, StarNodeOut } from "../../api";
import { StarNebula, applyDeepSpace, clearDeepSpace } from "./nebula";
import { STAR_FRAG, STAR_VERT, glowTexture, NODE_FRAG, NODE_VERT } from "./shaders";
import { createSketchPass, ensureDepthTexture } from "./sketch";

/** 星图节点（前端带坐标与展开状态）。 */
export interface SNode {
  id: string;
  name: string;
  type: "root" | "drive" | "kind" | "folder" | "file";
  path: string;
  kind: string;
  size: number;
  count: number;
  depth: number;
  mtime: number;
  parent: string | null;
  hasChildren: boolean;
  cluster: number;
  color: string;
  /** 是否已展开过子节点（钻取状态，需求 #20）。 */
  expanded: boolean;
  /** 由力导向写入。 */
  x: number;
  y: number;
  z: number;
  vx?: number;
  vy?: number;
  vz?: number;
  /** 固定坐标：轨道模式下一律锁死，否则 d3 会把天体又拽回去。 */
  fx?: number | null;
  fy?: number | null;
  fz?: number | null;
  __threeObj?: THREE.Object3D;
  index: number;
  /** 自转相位（需求 #5/#6），0~1，决定表面条纹从哪个角度起转。 */
  spin?: number;
  /** 恒星光谱型（system / data / archive / removable），决定核心与日冕配色。 */
  starClass?: string;
  /** 物理容量（字节）。恒星半径与引力都由它推导，而不是索引字节数。 */
  capacity?: number;
  /** 已用比例 0~1，画恒星外圈的进度光环。 */
  usedRatio?: number;
  /** 恒星引力强度 0~1：越大，行星轨道半径越大、公转越慢（需求 #5）。 */
  gravity?: number;
  /** 恒星耀斑强度 0~1：磁盘越活跃越强（需求 #6）。 */
  flare?: number;
  /** 轨道参数：由引擎按层级算出，用于「恒星自转 / 行星公转」。 */
  orbit?: OrbitSpec | null;
}

/**
 * 天体轨道（需求 #5「盘符变恒星要有引力」、#6「文件夹是行星也要自转公转」）。
 *
 * 为什么不用真实 N 体引力积分：星图里的天体是**数据节点**，不是物理粒子。
 * 真做 O(n²) 引力，展开一个目录（上百个节点）就会掉到个位数帧率，
 * 而且节点会被甩飞、互相吞并，反而看不出层级。
 * 这里用「解析圆轨道 + 相位均分」：视觉上是多体互绕、可预测、永不发散，
 * 并且每个子天体的中心每帧跟着父天体走，天然带上「被引力牵着」的观感。
 */
export interface OrbitSpec {
  /** 轨道中心的天体 id（恒星绕星系质心时为 null）。 */
  center: string | null;
  /** 轨道半径（xz 平面）。 */
  radius: number;
  /** 初相位。 */
  phase: number;
  /** 角速度（弧度/秒，正负表示方向）。 */
  speed: number;
  /** 轨道倾角（弧度）。 */
  tilt: number;
  /** 竖直方向的小幅浮动幅度，让轨道不是一个死平的圆盘。 */
  bob: number;
}

export interface SLink {
  source: SNode | string;
  target: SNode | string;
  type: string;
  strength: number;
}

export interface StarEngineHooks {
  onHover?: (node: SNode | null) => void;
  onSelect?: (node: SNode | null) => void;
  onActivate?: (node: SNode) => void;
  onMenu?: (node: SNode, x: number, y: number) => void;
  /** 相机/力导向每帧回调，用于刷新 HUD 与标签层。 */
  onFrame?: () => void;
  /** 节点被力导向移动后，返回该 id 的屏幕坐标，供标签层用。 */
  project?: (ids: string[]) => Map<string, { x: number; y: number; visible: boolean }>;
}

export interface StarEngineOptions {
  deepSpace?: boolean;
  bloom?: boolean;
  /** 目标帧率下限，低于时回调给宿主做降级（需求 #31）。 */
  onSlowFrame?: (fps: number) => void;
  dprOverride?: number;
  /** 铅绘滤镜（#24~#28）。 */
  sketch?: boolean;
}

const BG_PLAIN = 0x080b14;
/**
 * 传给 3d-force-graph 的背景色必须是 CSS 字符串。
 * 它内部会走 polished.parseToRgb —— 传数字会直接抛 PolishedError，整个 3D 视图白屏。
 * scene.background 那条路径（applyDeepSpace/clearDeepSpace）用数字没问题。
 */
const BG_DEEP_CSS = "#050510";

/**
 * 恒星色谱（需求 #1「不同盘符类型视觉区分明显」）。
 *
 * core 是球体本色，corona 是日冕色。两者刻意分开：核心偏白偏冷体现
 * "温度"，日冕偏暖体现"辐射"，叠在一起才有恒星的真实层次。
 * 与后端 `star.rs::star_class_color` 是同一套语义，但这里的 corona
 * 是后端没有的渲染细节，所以在前端单独定义。
 */
export const STAR_CLASS: Record<string, { core: string; corona: string; label: string }> = {
  // 系统盘：蓝白高温恒星
  system: { core: "#bcdcff", corona: "#4fa8ff", label: "系统盘" },
  // 仓库盘：橙红巨星
  data: { core: "#ffd9a0", corona: "#ff8a3d", label: "数据盘" },
  // 备份盘：暗红矮星
  archive: { core: "#ffb3a0", corona: "#c4503a", label: "归档盘" },
  // 移动盘：冷却白矮星
  removable: { core: "#d8f5e4", corona: "#4fd1a5", label: "移动盘" },
};

/** 未知类型时的兜底色（与数据盘同族，但更中性）。 */
const STAR_FALLBACK = { core: "#ffe2b8", corona: "#e8944a", label: "磁盘" };

export function starPalette(cls?: string) {
  return (cls && STAR_CLASS[cls]) || STAR_FALLBACK;
}

/** 力导向参数（需求 #12：alpha 1.0 / alphaDecay 0.0228 / velocityDecay 0.4）。 */
const FORCE = { alpha: 1.0, alphaDecay: 0.0228, velocityDecay: 0.4 };

/**
 * 设备档位探测（需求 #33）：触摸屏 / 小窗口 / 少核心 / 低内存 → 降配。
 * 只依赖浏览器免费给的信息，不去解析 GPU 型号字符串 —— 那个字符串在
 * 各家驱动上格式混乱，正则匹配出来的结论还不如核数可靠。
 */
export function detectTier(): "low" | "mid" | "high" {
  if (typeof navigator === "undefined" || typeof window === "undefined") return "high";
  const nav = navigator as any;
  const cores = Number(nav.hardwareConcurrency) || 8;
  const mem = Number(nav.deviceMemory) || 0;
  const coarse = window.matchMedia?.("(pointer: coarse)")?.matches ?? false;
  const small = Math.min(window.innerWidth || 1920, window.innerHeight || 1080) < 720;
  if (mem && mem <= 2) return "low";
  if (cores <= 2) return "low";
  if (coarse && (small || cores <= 4)) return "low";
  if (coarse || small || cores <= 4 || (mem && mem <= 4)) return "mid";
  return "high";
}

/**
 * 容量 → 恒星半径。与后端 `disk::star_radius_of` 保持同一公式。
 *
 * 为什么前端还要再算一遍：从后端拿到的是索引快照那一刻的容量，
 * 而用户在星图里插拔 U 盘、点"刷新"时应该立刻反映；而且"未索引的盘"
 * 根本没有后端节点，需要前端自己按 star_drives 补一颗灰态恒星。
 * 两处公式必须一致，否则同一块盘在两种来源下大小会跳。
 */
export function starRadiusOf(totalBytes: number): number {
  const gb = Math.max(0, totalBytes) / (1024 * 1024 * 1024);
  // 【尺度必须跟着场景走】初版用 28 + cbrt(GB)*61，1.8TB 算出 770 ——
  // 而 3D 星图的场景尺度是"相机距离 1000、恒星互绕半径 150~300"，
  // 770 的恒星半径比整个星系还大，四张 billboard 直接把视口铺满，
  // 屏幕上只剩一块褐色矩形（实测截图就是这样）。
  // 20 + cbrt(GB)*8.5：16GB=41、1TB=106、8TB=190，与 150~300 的轨道尺度匹配。
  return 20 + Math.cbrt(gb) * 8.5;
}

/** 容量 → 引力强度 0~1。恒星越大，行星轨道越远、公转越慢。 */
export function gravityOf(totalBytes: number): number {
  const gb = Math.max(0, totalBytes) / (1024 * 1024 * 1024);
  const raw = Math.cbrt(gb) * 61;
  return 0.12 + (raw / (raw + 120)) * 0.88;
}

/** 稳定的字符串散列（FNV-1a），用于给天体分配确定性的相位/倾角。 */
function hash32(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

export class StarEngine3D {
  readonly fg: any;
  private el: HTMLElement;
  private scene: THREE.Scene;
  private camera: THREE.Camera;
  private renderer: THREE.WebGLRenderer;

  private nodeGeom: THREE.InstancedBufferGeometry;
  private nodeMat: THREE.ShaderMaterial;
  private nodeMesh: THREE.Mesh;
  private nodeCount = 0;

  /** 恒星层（盘符 = 恒星）。与普通节点分开渲染，用专用球体着色器。 */
  private starGeom: THREE.InstancedBufferGeometry;
  private starMat: THREE.ShaderMaterial;
  private starMesh: THREE.Mesh;
  /** 恒星在 nodes 数组里的下标，每帧同步坐标时只更新这几个。 */
  private starIdx: number[] = [];
  private starOff = new Float32Array(0);
  private starCol = new Float32Array(0);
  private starCorona = new Float32Array(0);
  private starSize = new Float32Array(0);
  private starAlpha = new Float32Array(0);
  private starFlare = new Float32Array(0);
  private starSeed = new Float32Array(0);

  private linkGeom: THREE.BufferGeometry;
  private linkMat: THREE.LineBasicMaterial;
  private linkLines: THREE.LineSegments;

  private nebula: StarNebula | null = null;
  private bloomPass: UnrealBloomPass | null = null;
  private sketchPass: any | null = null;

  private nodes: SNode[] = [];
  private links: SLink[] = [];
  private byId = new Map<string, SNode>();

  /** 位置/颜色/尺寸的 JS 侧镜像，避免每帧读 GPU 缓冲。 */
  private offArr = new Float32Array(0);
  private colArr = new Float32Array(0);
  private sizeArr = new Float32Array(0);
  private alphaArr = new Float32Array(0);
  private linkPos = new Float32Array(0);
  /** 线段 → 逻辑边下标、平行偏移、以及两端在 a→b 上的参数（#6 线宽 / #22 虚线）。 */
  private segEdge: Int32Array = new Int32Array(0);
  private segShift: Float32Array = new Float32Array(0);
  private segTa: Float32Array = new Float32Array(0);
  private segTb: Float32Array = new Float32Array(0);
  private segCount = 0;
  /** LOD（#30）：每节点按到相机距离的尺寸系数。 */
  private lodArr = new Float32Array(0);
  /** 自转相位（需求 #5/#6）：每个天体的表面条纹起始角度。 */
  private spinArr = new Float32Array(0);
  /** 轨道时钟（秒），所有公转共用，保证相位一致。 */
  private orbitTime = 0;
  /** 参与轨道的节点，按 depth 升序排好，保证父天体先更新再算子天体。 */
  private orbitOrder: SNode[] = [];
  /** 交互期临时关闭后处理（#32）。 */
  private postSuspended = false;
  private resumeTimer = 0;
  private lodTimer = 0;
  /** 切到后台就停渲染（#33）。 */
  private onVisibility = () => {
    if (typeof document === "undefined") return;
    if (document.hidden) {
      this.fg.pauseAnimation?.();
    } else {
      this.fg.resumeAnimation?.();
      this.lastT = 0;
    }
  };

  private hovered: SNode | null = null;
  private selected: SNode | null = null;
  private neighborSet = new Set<string>();
  /** 被图例过滤掉的节点类型（#5 图例 / #22 标签过滤）：隐藏而不是删除，布局不跳。 */
  private hiddenKinds = new Set<string>();
  /** 当前钻取聚光的节点（#19）。 */
  private spotId: string | null = null;

  private raf = 0;
  private lastT = 0;
  private frameTimes: number[] = [];
  private slowStreak = 0;
  private disposed = false;

  private opts: StarEngineOptions;
  private hooks: StarEngineHooks;
  /** 设备档位（需求 #33）：决定星尘数量、Bloom 强度、像素比上限。 */
  private tier: "low" | "mid" | "high" = "high";

  /** 相机当前看向的焦点，用于「返回上一级」时恢复视角。 */
  private focusId: string | null = null;

  constructor(el: HTMLElement, hooks: StarEngineHooks = {}, opts: StarEngineOptions = {}) {
    this.el = el;
    this.hooks = hooks;
    this.opts = opts;
    this.tier = detectTier();
    const w = Math.max(1, el.clientWidth);
    const h = Math.max(1, el.clientHeight);

    this.fg = new (ForceGraph3D as any)(el, {
      controlType: "orbit",
      rendererConfig: { antialias: true, alpha: false, powerPreference: "high-performance" },
    })
      .width(w)
      .height(h)
      // 3d-force-graph 会把背景塞给 polished 解析，数字会被判为非法颜色并抛错（整个 3D 视图白屏）。
      // 背景统一交给 scene.background（applyDeepSpace/clearDeepSpace）处理，这里只给合法的 CSS 色串。
      .backgroundColor(BG_DEEP_CSS)
      .showNavInfo(false)
      // 自带节点/连线全部关掉，只管算力与相机。
      .nodeVisibility(() => false)
      .linkVisibility(() => false)
      .nodeLabel(() => "")
      .d3AlphaDecay(FORCE.alphaDecay)
      .d3VelocityDecay(FORCE.velocityDecay)
      .onEngineTick(() => this.syncPositions())
      .onEngineStop(() => this.syncPositions());

    this.scene = this.fg.scene();
    this.camera = this.fg.camera();
    this.renderer = this.fg.renderer();

    // ---- 节点层：1 次 draw call ---------------------------------------
    const quad = new THREE.PlaneGeometry(2, 2);
    this.nodeGeom = new THREE.InstancedBufferGeometry();
    this.nodeGeom.index = quad.index;
    this.nodeGeom.attributes.position = quad.attributes.position;
    this.nodeGeom.attributes.uv = quad.attributes.uv;
    this.nodeGeom.instanceCount = 0;

    this.nodeMat = new THREE.ShaderMaterial({
      uniforms: {
        uGlow: { value: glowTexture() },
        // LOD（#30）：远处节点压暗压小，近处高精度；由 JS 侧按相机距离写 aLod。
        uLodScale: { value: 1 },
        // 自转（需求 #5/#6）：天体表面条纹的行进相位。
        uTime: { value: 0 },
      },
      vertexShader: NODE_VERT,
      fragmentShader: NODE_FRAG,
      transparent: true,
      depthWrite: false,
      depthTest: true,
      blending: THREE.AdditiveBlending,
      fog: false,
    });
    this.nodeMesh = new THREE.Mesh(this.nodeGeom, this.nodeMat);
    this.nodeMesh.frustumCulled = false;
    this.nodeMesh.name = "star-nodes";
    this.scene.add(this.nodeMesh);

    // ---- 恒星级层：单独一份实例几何 + 专用球体/日冕着色器 ----------------
    // 恒星只有几个盘符，数量固定在个位数，所以可以给它更贵的着色：
    // 球体温度梯度 + 日冕 + 表面耀斑。仍然只有 1 次 draw call。
    this.starGeom = new THREE.InstancedBufferGeometry();
    this.starGeom.index = quad.index;
    this.starGeom.attributes.position = quad.attributes.position;
    this.starGeom.attributes.uv = quad.attributes.uv;
    this.starGeom.instanceCount = 0;
    this.starMat = new THREE.ShaderMaterial({
      uniforms: { uTime: { value: 0 } },
      vertexShader: STAR_VERT,
      fragmentShader: STAR_FRAG,
      transparent: true,
      depthWrite: false,
      depthTest: true,
      blending: THREE.AdditiveBlending,
      fog: false,
    });
    this.starMesh = new THREE.Mesh(this.starGeom, this.starMat);
    this.starMesh.frustumCulled = false;
    this.starMesh.name = "star-bodies";
    this.starMesh.renderOrder = 1;
    this.scene.add(this.starMesh);

    // ---- 连线层：1 次 draw call ---------------------------------------
    this.linkGeom = new THREE.BufferGeometry();
    this.linkGeom.setAttribute("position", new THREE.BufferAttribute(new Float32Array(0), 3));
    this.linkMat = new THREE.LineBasicMaterial({
      vertexColors: true,
      transparent: true,
      // 整体透明度交给顶点色的 alpha 分量控制（关系强度，需求 #6），
      // 材质自身保持 1，避免再乘一次把强弱差压平。
      opacity: 1,
      blending: THREE.AdditiveBlending,
      depthWrite: false,
    });
    this.linkLines = new THREE.LineSegments(this.linkGeom, this.linkMat);
    this.linkLines.frustumCulled = false;
    this.linkLines.name = "star-links";
    this.scene.add(this.linkLines);

    // 纹理采样质量（需求 #31）：grow 贴图与星云贴图都是运行时生成的，
    // 默认就有 mipmap（CanvasTexture 是 2 的幂尺寸）；再补上各向异性过滤，
    // 斜看星云时远处才不会糊成一片色块。
    const maxAniso = this.renderer.capabilities?.getMaxAnisotropy?.() ?? 1;
    const glow = this.nodeMat.uniforms.uGlow.value as THREE.Texture;
    glow.anisotropy = maxAniso;
    glow.generateMipmaps = true;

    this.applySceneMode();
    this.applyBloom();
    this.applySketch();
    this.applyDpr();

    // 调试挂钩：CDP 里可直接读引擎内部状态（排障时唯一入口）。
    if (typeof window !== "undefined") (window as any).__ddoStarEngine = this;
    this.bindPointer();
    // 切到后台就停渲染，回来再继续（需求 #33：省电，也不让隐藏窗口烧 GPU）。
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", this.onVisibility);
    }
    this.startLoop();
  }

  // ------------------------------------------------------------ 场景

  private applySceneMode() {
    if (this.opts.deepSpace) {
      applyDeepSpace(this.scene);
      if (!this.nebula) {
        // 档位决定星尘数量（#33：低端设备 1200 颗足够铺满背景）。
        const count = this.tier === "low" ? 1200 : this.tier === "mid" ? 2200 : 3000;
        this.nebula = new StarNebula({ count, radius: 900 });
        this.nebula.setPixelRatio(this.currentDpr());
        this.scene.add(this.nebula.group);
      }
      this.nebula.group.visible = true;
      this.fg.backgroundColor(BG_DEEP_CSS);
    } else {
      this.nebula && (this.nebula.group.visible = false);
      clearDeepSpace(this.scene, BG_PLAIN);
    }
  }

  /**
   * Bloom（需求 #10）。
   *
   * 需求给的基准是 strength 1.8 / radius 0.5 / threshold 0.1，但 threshold 0.1
   * 意味着"几乎整个画面都参与泛光"：深空底色 0x050510 换算成亮度已经接近 0.1，
   * 星云内壁又是大面积中低亮度渐变，结果是背景自己发光、叠完直接白屏。
   * 这里把 threshold 抬到 0.62，让泛光只落在真正的发光体（节点亮芯、星尘高光）上，
   * strength 相应降到 0.85/0.7/0.55 —— 观感上的"辉光"仍然在，
   * 但不再是整屏过曝。
   */
  private applyBloom() {
    const composer = this.fg.postProcessingComposer?.();
    if (!composer) return;
    if (this.opts.bloom) {
      if (!this.bloomPass) {
        // 恒星重做之后画面里的高光更集中（几颗大恒星 vs 原来的小光点），
        // 阈值提到 0.74 让泛光只吃恒星的核心与日冕内圈；
        // strength 相应降到 0.72/0.6/0.48，避免大恒星边缘糊成一团白。
        const strength = this.tier === "low" ? 0.48 : this.tier === "mid" ? 0.6 : 0.72;
        this.bloomPass = new UnrealBloomPass(
          new THREE.Vector2(this.el.clientWidth || 1, this.el.clientHeight || 1),
          strength,
          0.32,
          0.74,
        );
        composer.addPass(this.bloomPass);
      }
      this.bloomPass.enabled = !this.postSuspended;
    } else if (this.bloomPass) {
      this.bloomPass.enabled = false;
    }
  }

  /**
   * 铅绘滤镜（#24~#28）：作为 composer 的最后一个 pass 挂在 Bloom 之后。
   * 默认关闭 —— 它是"看着玩"的效果，不能影响正常检索的观感。
   */
  private applySketch() {
    const composer = this.fg.postProcessingComposer?.();
    if (!composer) return;
    const w = Math.max(2, Math.floor(this.el.clientWidth));
    const h = Math.max(2, Math.floor(this.el.clientHeight));
    if (!this.sketchPass) {
      this.sketchPass = createSketchPass(w, h);
      this.sketchPass.enabled = false;
      composer.addPass(this.sketchPass);
    }
    this.sketchPass.enabled = !!this.opts.sketch && !this.postSuspended;
    if (this.opts.sketch) {
      const depth = ensureDepthTexture(composer, w * this.currentDpr(), h * this.currentDpr());
      if (depth) this.sketchPass.uniforms.tDepth.value = depth;
      this.sketchPass.uniforms.uResolution.value.set(w, h);
    }
  }

  private currentDpr(): number {
    const device = window.devicePixelRatio || 1;
    // 档位上限（#33）：低端 1x、中端 1.5x、高端 2x；用户手动覆盖可到 3x，
    // 但手动设置不该被档位悄悄压掉，所以覆盖优先。
    const tierCap = this.tier === "low" ? 1 : this.tier === "mid" ? 1.5 : 2;
    const cap = this.opts.dprOverride && this.opts.dprOverride > 0
      ? Math.min(3, this.opts.dprOverride)
      : Math.min(tierCap, device);
    return Math.max(1, cap);
  }

  /**
   * 像素比（需求 #31）：three-render-objects 内部把 dpr 硬钳到 2，
   * 想上到 3 倍只能在它 resize 之后再覆盖一次。这里每帧兜底写入，
   * 因为它会在每次 resize 时重设成 min(2, dpr)。
   */
  private applyDpr() {
    const dpr = this.currentDpr();
    const r = this.renderer;
    if (Math.abs(r.getPixelRatio() - dpr) > 0.01) {
      r.setPixelRatio(dpr);
      this.fg.width(this.el.clientWidth);
      this.fg.height(this.el.clientHeight);
    }
    this.nebula?.setPixelRatio(dpr);
    this.nebula?.setAnisotropy(this.renderer.capabilities?.getMaxAnisotropy?.() ?? 1);
  }

  // ------------------------------------------------------------ 数据

  setData(nodes: StarNodeOut[], edges: StarEdgeOut[]) {
    this.nodes = nodes.map((n, i) => ({
      id: n.id,
      name: n.name,
      type: (n.node_type as SNode["type"]) || "file",
      path: n.path,
      kind: n.kind,
      size: n.size,
      count: n.count,
      depth: n.depth,
      mtime: n.mtime,
      parent: n.parent,
      hasChildren: n.has_children,
      cluster: n.cluster,
      color: n.color,
      expanded: false,
      x: 0,
      y: 0,
      z: 0,
      index: i,
      starClass: n.star_class,
      capacity: n.capacity,
      usedRatio: n.used_ratio,
      gravity: n.capacity ? gravityOf(n.capacity) : undefined,
      flare: 0,
    }));
    this.byId = new Map(this.nodes.map((n) => [n.id, n]));
    this.links = edges
      .map((e) => ({ source: e.source, target: e.target, type: e.edge_type, strength: e.strength }))
      .filter((e) => this.byId.has(e.source) && this.byId.has(e.target));

    // 全量重建：先按层级分配轨道，再由轨道解析式直接算出第一帧位置。
    // 不再用球面黄金分布 —— 那是给纯力导向用的初值，轨道模式下会让
    // 第一帧的天体全挤在球壳上再"飞回"各自轨道，看起来像炸开。
    this.assignOrbits();
    this.orbitTime = 0;
    this.updateOrbits(0, true);

    this.rebuildBuffers();
    this.pushGraph();
    // 数据第一次落位后按恒星分布取景：否则相机停在默认的 1000 距，
    // 而恒星轨道半径已经到 570，两颗盘会占满整个视口。
    this.frameStars(0);
  }

  /** 增量加入一批子节点（钻取，需求 #20）。 */
  addData(nodes: StarNodeOut[], edges: StarEdgeOut[], parent?: SNode | null) {
    const fresh: SNode[] = [];
    const startIndex = this.nodes.length;
    for (const n of nodes) {
      if (this.byId.has(n.id)) continue;
      const sn: SNode = {
        id: n.id,
        name: n.name,
        type: (n.node_type as SNode["type"]) || "file",
        path: n.path,
        kind: n.kind,
        size: n.size,
        count: n.count,
        depth: n.depth,
        mtime: n.mtime,
        parent: n.parent,
        hasChildren: n.has_children,
        cluster: n.cluster,
        color: n.color,
        expanded: false,
        x: 0,
        y: 0,
        z: 0,
        index: startIndex + fresh.length,
        starClass: n.star_class,
        capacity: n.capacity,
        usedRatio: n.used_ratio,
        gravity: n.capacity ? gravityOf(n.capacity) : undefined,
        flare: 0,
      };
      // 新节点从父节点位置"长出来"，下一帧就会被轨道拉到自己的轨道位。
      if (parent) {
        // 轨道运动会让坐标在选中后继续变化，所以永远看 byId 里的实时对象，
        // 不能用外面传进来的 parent 快照。
        const live = this.byId.get(parent.id) ?? parent;
        sn.x = live.x;
        sn.y = live.y;
        sn.z = live.z;
      }
      fresh.push(sn);
      this.byId.set(sn.id, sn);
    }
    if (!fresh.length) return fresh;
    this.nodes = this.nodes.concat(fresh);
    for (const e of edges) {
      if (!this.byId.has(e.source) || !this.byId.has(e.target)) continue;
      if (this.links.some((l) => this.idOf(l.source) === e.source && this.idOf(l.target) === e.target)) continue;
      this.links.push({ source: e.source, target: e.target, type: e.edge_type, strength: e.strength });
    }
    this.assignOrbits();
    // 增量展开时**不做 instant 落位**：已有行星的轨道不变，不该被重新摆一遍；
    // 新行星从父体位置出发，靠 updateOrbits 的 0.5 插值"长出去"，
    // 观感上是平滑展开，而不是"啪"地多出一圈（需求 #6）。
    this.updateOrbits(0, false);
    this.rebuildBuffers();
    this.pushGraph();
    return fresh;
  }

  private idOf(v: SNode | string): string {
    return typeof v === "string" ? v : v.id;
  }

  // ------------------------------------------------------------ 轨道

  /**
   * 给每个天体分配轨道（需求 #5「盘符变恒星，要有引力」/#6「文件夹行星自转公转」）。
   *
   * 引力怎么表达？不是算 N 体方程，而是让**子天体的位置由父天体决定**：
   * 每帧先算父天体，再以它的实时坐标为圆心摆小时体。父天体一动，
   * 整个子星系跟着平移 —— 观感上就是被引力牵着走，且永远不会发散。
   *
   * 层级 → 轨道：
   *  - 1 颗恒星：孤零零一颗，自转（speed 0，绕自己转）。
   *  - N 颗恒星（N 个盘）：共用一个质心，按 N 体运动，各自以相同角速度
   *    绕质心转一个圆 —— 正是用户说的「两颗双星、三颗三体、四颗四体」。
   *    真实三体是混沌的、会甩飞；这里用等相位圆轨道，看起来仍是相互绕转。
   *  - 行星（kind）：绕所属恒星公转，轨道半径按恒星尺寸 + 序号铺开。
   *  - 卫星/文件：绕所属行星公转，半径更小、角速度更快。
   */
  private assignOrbits() {
    const byId = this.byId;
    // 同层兄弟数决定相位均分，避免所有天体排成一条直线。
    const siblingCount = new Map<string, number>();
    for (const n of this.nodes) {
      const key = n.parent ?? "";
      siblingCount.set(key, (siblingCount.get(key) ?? 0) + 1);
    }
    const siblingIndex = new Map<string, number>();

    // 恒星（drive）两两互绕：按数量算出共用的质心轨道半径。
    const drives = this.nodes.filter((n) => n.type === "drive");
    const driveCount = drives.length;

    for (const n of this.nodes) {
      n.spin = Math.random();
      const key = n.parent ?? "";
      const idx = siblingIndex.get(key) ?? 0;
      siblingIndex.set(key, idx + 1);
      const total = Math.max(1, siblingCount.get(key) ?? 1);
      const phase = (idx / total) * Math.PI * 2 + (hash32(n.id) % 628) / 100;

      if (n.type === "drive") {
        if (driveCount <= 1) {
          // 单盘：一颗恒星孤悬，自转，不公转（需求 #5「只有一个盘一颗恒星自转」）。
          n.orbit = { center: null, radius: 0, phase: 0, speed: 0, tilt: 0, bob: 0 };
        } else {
          // 多盘：绕公共质心。角速度随盘数增加而变慢 —— 天体越多，
          // 系统越"重"，看起来更有质量感，也避免四体时转得眼花。
          //
          // 轨道半径按**容量**加权：两颗盘若容量差很多，小盘会被大盘"拽"
          // 着绕大圈（质心偏移的观感），而不是像原来那样两个等半径硬转。
          // 轨道半径必须**跟着恒星尺寸**走，不能是固定值。
          //
          // 【踩坑】原来是 (150 + n*46) * 容量系数，与恒星半径毫无关系。
          // 恒星半径改成按容量算之后，1.8TB 的盘半径 123，两颗恒星的
          // 轨道半径仍是 242 —— 圆心距只有 484，而两颗直径合计 244 已经
          // 占了一半，加上日冕（2.6 倍）直接叠成一团白光。
          // 现在按"最大恒星日冕半径的 2.6 倍"起算，两颗永远分得开。
          const maxStarR = Math.max(...drives.map((d) => this.radiusOf(d)));
          const nebulaGap = maxStarR * 2.6;
          const caps = drives.map((d) => d.capacity ?? 0);
          const maxCap = Math.max(1, ...caps);
          const avgCap = caps.reduce((a, b) => a + b, 0) / Math.max(1, caps.length);
          const massScale = Math.pow(Math.max(0.35, avgCap / maxCap), 0.5);
          // 圆心距 = 2r，两颗恒星之间至少留 1.2 个日冕直径的间隙。
          const r = nebulaGap * (1.0 + driveCount * 0.34) * (0.85 + massScale * 0.3);
          const speed = (0.16 / Math.sqrt(driveCount)) * (0.6 + massScale * 0.7);
          // 正反两个方向交替，成对时就是标准的双星互绕。
          const dir = idx % 2 === 0 ? 1 : -1;
          n.orbit = {
            center: null,
            radius: r,
            phase: (idx / driveCount) * Math.PI * 2,
            speed: speed * dir,
            tilt: (idx % 2 === 0 ? 1 : -1) * 0.12,
            bob: 18,
          };
        }
        continue;
      }

      if (n.type === "root") {
        n.orbit = null;
        continue;
      }

      // 其余天体绕父天体转。
      const parent = n.parent ? byId.get(n.parent) : null;
      if (!parent) {
        n.orbit = null;
        continue;
      }
      const pr = this.radiusOf(parent);
      // 行星（kind）绕恒星：半径 = 恒星半径 * (2.2 + 0.55*序号)，
      // 保证相邻行星不会叠在一起，也不会跑到画面外。
      // 卫星/文件绕行星：轨道更紧凑。
      const ring = n.type === "kind" ? 2.4 + idx * 0.62 : 1.7 + (idx % 8) * 0.34;
      // 引力（需求「跟随盘符大小改变引力」）的物理表达：
      // 恒星越大 → 引力越强 → 行星被"抓得更牢"，轨道半径相对恒星尺寸更大、
      // 公转角速度更快。旧实现对所有盘用同一个 ring 系数，2TB 的盘和 64GB 的
      // U 盘看起来一模一样，用户说的"引力不变"就是这个。
      const g = parent.type === "drive" ? (parent.gravity ?? 0.35) : 0.5;
      // 引力 0.12~1 映射到 0.72~1.55 的轨道放大：小盘的卫星几乎贴着表面，
      // 大盘的卫星被甩出去老远 —— 一眼就能看出"这颗恒星更重"。
      const pull = 0.72 + g * 0.83;
      const radius = pr * ring * pull + (n.type === "kind" ? 26 : 6);
      // 内圈快、外圈慢，是同向公转应有的样子（开普勒式）。
      const base = n.type === "kind" ? 0.34 : 0.62;
      // 引力越强，公转越快（近恒星的轨道周期本来就短）。
      const speed =
        (base / Math.sqrt(Math.max(0.4, radius / 30))) *
        (0.55 + g * 0.9) *
        (idx % 2 === 0 ? 1 : -1) *
        0.5;
      n.orbit = {
        center: parent.id,
        radius,
        phase,
        speed: speed + 0.08 * (idx % 2 === 0 ? 1 : -1),
        tilt: ((hash32(n.id) % 100) / 100 - 0.5) * 0.55,
        bob: n.type === "kind" ? 10 : 4,
      };
    }

    // 父天体必须先算，子天体才能挂在正确的位置上。
    // 不能信后端给的 depth：star.rs 里子目录恒为 0、文件恒为 1（历史遗留），
    // 直接排序会让"子目录"排到它自己的父节点前面。这里按 parent 关系做 BFS。
    this.orbitOrder = [];
    const childrenOf = new Map<string, SNode[]>();
    for (const n of this.nodes) {
      const k = n.parent ?? "";
      const arr = childrenOf.get(k);
      if (arr) arr.push(n);
      else childrenOf.set(k, [n]);
    }
    const queue: SNode[] = [];
    for (const n of this.nodes) {
      // 没有父节点、或父节点不在图里的，都是一棵子树的根。
      if (!n.parent || !byId.has(n.parent)) queue.push(n);
    }
    // 按队列展开，天然保证父在天体数组里的位置早于子。
    const seen = new Set<string>();
    while (queue.length) {
      const n = queue.shift() as SNode;
      if (seen.has(n.id)) continue;
      seen.add(n.id);
      this.orbitOrder.push(n);
      const kids = childrenOf.get(n.id);
      if (kids) for (const k of kids) queue.push(k);
    }
    // 兜底：成环或孤儿（父节点已被删）的节点也必须参与更新，否则会卡在原地。
    for (const n of this.nodes) if (!seen.has(n.id)) this.orbitOrder.push(n);
  }

  /**
   * 按轨道解析式更新坐标。
   *
   * @param dt 秒；0 表示只重算不动时间（重建/回灌布局时用）
   * @param instant 立刻写坐标，不做平滑
   */
  private updateOrbits(dt: number, instant = false) {
    if (dt > 0) this.orbitTime += dt;
    const t = this.orbitTime;
    // 没有轨道的天体（root）留在原点。
    for (const n of this.orbitOrder) {
      const o = n.orbit;
      if (!o) continue;
      let cx = 0;
      let cy = 0;
      let cz = 0;
      if (o.center) {
        const p = this.byId.get(o.center);
        if (!p) continue;
        cx = p.x;
        cy = p.y;
        cz = p.z;
      }
      const a = o.phase + t * o.speed;
      // 圆轨道在 xz 平面，再按 tilt 绕 x 轴倾斜，bob 给出竖直浮动。
      const ox = Math.cos(a) * o.radius;
      const oz = Math.sin(a) * o.radius;
      const oy = Math.sin(a * 1.7 + o.phase) * o.bob;
      const ct = Math.cos(o.tilt);
      const st = Math.sin(o.tilt);
      const x = cx + ox;
      const y = cy + oy * ct - oz * st;
      const z = cz + oz * ct + oy * st;
      if (instant) {
        n.x = x;
        n.y = y;
        n.z = z;
      } else {
        // 小幅插值：轨道是解析的，本来就不会抖；插值只为让新增节点的
        // "长出来"有一点点惯性，不会瞬移。
        n.x += (x - n.x) * 0.5;
        n.y += (y - n.y) * 0.5;
        n.z += (z - n.z) * 0.5;
      }
      // 让 d3 别插手：坐标由轨道决定。
      n.fx = n.x;
      n.fy = n.y;
      n.fz = n.z;
    }
  }

  /** 轨道模式下 d3 只负责建索引与拾取，物理一律关掉。 */
  private applyOrbitForces() {
    const fg: any = this.fg;
    // 只关强度，不碰 distance/iterations —— 3d-force-graph 内部仍会跑
    // simulation 的 tick，但节点有 fx/fy/fz 锁死，坐标不会被它改掉。
    fg.d3Force?.("charge")?.strength?.(0);
    fg.d3Force?.("link")?.strength?.(0);
  }

  private rebuildBuffers() {
    const n = this.nodes.length;
    this.offArr = new Float32Array(n * 3);
    this.colArr = new Float32Array(n * 3);
    this.sizeArr = new Float32Array(n);
    this.alphaArr = new Float32Array(n);
    this.lodArr = new Float32Array(n);
    this.lodArr.fill(1);
    this.spinArr = new Float32Array(n);

    const c = new THREE.Color();
    for (let i = 0; i < n; i++) {
      const node = this.nodes[i];
      node.index = i;
      c.set(node.color || "#9C755F");
      this.colArr[i * 3] = c.r;
      this.colArr[i * 3 + 1] = c.g;
      this.colArr[i * 3 + 2] = c.b;
      // 恒星由独立的恒星层渲染（有体积的球 + 日冕），这里把它的普通光点
      // 尺寸压到 0 —— 否则同一个位置会既有球又有扁平光斑，叠出来是个白团。
      this.sizeArr[i] = node.type === "drive" ? 0 : this.radiusOf(node);
      this.alphaArr[i] = 1;
      this.spinArr[i] = node.spin ?? 0;
    }

    const g = this.nodeGeom;
    g.setAttribute("aOffset", new THREE.InstancedBufferAttribute(this.offArr, 3));
    g.setAttribute("aColor", new THREE.InstancedBufferAttribute(this.colArr, 3));
    g.setAttribute("aSize", new THREE.InstancedBufferAttribute(this.sizeArr, 1));
    g.setAttribute("aAlpha", new THREE.InstancedBufferAttribute(this.alphaArr, 1));
    g.setAttribute("aLod", new THREE.InstancedBufferAttribute(this.lodArr, 1));
    g.setAttribute("aSpin", new THREE.InstancedBufferAttribute(this.spinArr, 1));
    g.instanceCount = n;
    // 排障用快照：真 GUI 出问题时，CDP 里读 window.__ddoStarEngine._nodeDebug
    // 就能知道"每个节点是什么类型、算出多大"，不用重新加日志再打包。
    // 数据量固定为前 12 条，开销可忽略。
    (this as any)._nodeDebug = this.nodes.map((nd) => ({
      n: nd.name, t: nd.type, s: Math.round(this.sizeArr[nd.index] ?? 0),
    })).slice(0, 12);
    this.updateLod();

    this.rebuildStars();
    this.buildLinkSegments();
  }

  /**
   * 重建恒星级实例缓冲（盘符 = 恒星，需求 #1）。
   *
   * 与普通节点分开的原因：着色器不同（球体 + 日冕 vs 扁平光点），
   * 而且恒星数量是个位数、需要"每种光谱型不同配色"。合在一个缓冲里
   * 也能做，但要在片元里分支判断类型，白白让上万像素走昂贵分支。
   */
  private rebuildStars() {
    const idx: number[] = [];
    for (let i = 0; i < this.nodes.length; i++) {
      if (this.nodes[i].type === "drive") idx.push(i);
    }
    this.starIdx = idx;
    const m = idx.length;
    this.starOff = new Float32Array(m * 3);
    this.starCol = new Float32Array(m * 3);
    this.starCorona = new Float32Array(m * 3);
    this.starSize = new Float32Array(m);
    this.starAlpha = new Float32Array(m);
    this.starFlare = new Float32Array(m);
    this.starSeed = new Float32Array(m);

    const c = new THREE.Color();
    const cor = new THREE.Color();
    for (let k = 0; k < m; k++) {
      const n = this.nodes[idx[k]];
      const pal = starPalette(n.starClass);
      c.set(pal.core);
      cor.set(pal.corona);
      this.starCol[k * 3] = c.r;
      this.starCol[k * 3 + 1] = c.g;
      this.starCol[k * 3 + 2] = c.b;
      this.starCorona[k * 3] = cor.r;
      this.starCorona[k * 3 + 1] = cor.g;
      this.starCorona[k * 3 + 2] = cor.b;
      this.starSize[k] = this.radiusOf(n);
      this.starAlpha[k] = 1;
      // 耀斑默认 0.25：盘符空闲时也不能完全静止，会显得像块贴图。
      this.starFlare[k] = n.flare ?? 0.25;
      this.starSeed[k] = (hash32(n.id) % 1000) / 1000;
    }

    const g = this.starGeom;
    g.setAttribute("aOffset", new THREE.InstancedBufferAttribute(this.starOff, 3));
    g.setAttribute("aColor", new THREE.InstancedBufferAttribute(this.starCol, 3));
    g.setAttribute("aCorona", new THREE.InstancedBufferAttribute(this.starCorona, 3));
    g.setAttribute("aSize", new THREE.InstancedBufferAttribute(this.starSize, 1));
    g.setAttribute("aAlpha", new THREE.InstancedBufferAttribute(this.starAlpha, 1));
    g.setAttribute("aFlare", new THREE.InstancedBufferAttribute(this.starFlare, 1));
    g.setAttribute("aSeed", new THREE.InstancedBufferAttribute(this.starSeed, 1));
    g.instanceCount = m;
    (this as any)._starDebug = {
      count: m,
      sizes: Array.from(this.starSize).map((v) => Math.round(v)),
      total: this.nodes.length,
    };
  }

  /** 恒星坐标同步（与普通节点分开写，避免每帧遍历上万节点）。 */
  private syncStars() {
    if (!this.starIdx.length) return;
    for (let k = 0; k < this.starIdx.length; k++) {
      const n = this.nodes[this.starIdx[k]];
      this.starOff[k * 3] = n.x || 0;
      this.starOff[k * 3 + 1] = n.y || 0;
      this.starOff[k * 3 + 2] = n.z || 0;
    }
    const attr = this.starGeom.getAttribute("aOffset") as THREE.InstancedBufferAttribute | undefined;
    if (attr) attr.needsUpdate = true;
  }

  /**
   * 恒星活跃度 → 耀斑强度（需求 #6「盘符 IO 高时表面耀斑爆发」）。
   *
   * 后端没有实时 IO 采样接口，这里用「本次会话里该盘节点的变化量」近似：
   * 星图每展开一层都会 addData，涉及到的盘就闪一下。比伪造随机数诚实得多
   * —— 它确实反映了"这个盘正在被查看/改动"。
   */
  pulseStar(driveId: string, amount = 0.85) {
    const n = this.byId.get(driveId);
    if (!n || n.type !== "drive") return;
    n.flare = Math.min(1, Math.max(n.flare ?? 0.25, amount));
    const k = this.starIdx.indexOf(n.index);
    if (k >= 0) {
      this.starFlare[k] = n.flare;
      const a = this.starGeom.getAttribute("aFlare") as THREE.InstancedBufferAttribute | undefined;
      if (a) a.needsUpdate = true;
    }
  }

  /**
   * 连线几何（需求 #6 线宽 / #22 标签虚线）。
   *
   * WebGL 的 LineBasicMaterial.linewidth 在所有主流平台都被钳到 1px，
   * 想让「关系越强线越粗」真的看得见，只能用多条平行线段叠出宽度：
   * strength→宽度 1~3px，按 0.45px 间距铺开；标签类关系默认画成虚线。
   * 这样全线仍然只有 1 次 draw call。
   */
  private buildLinkSegments() {
    const nEdge = this.links.length;
    let segs = 0;
    for (const l of this.links) segs += this.edgeTracks(l.strength, l.type) * this.edgeDashes(l.type);
    this.segCount = segs;
    this.linkPos = new Float32Array(segs * 6);
    this.segEdge = new Int32Array(segs);
    this.segShift = new Float32Array(segs);
    this.segTa = new Float32Array(segs);
    this.segTb = new Float32Array(segs);
    // 4 分量顶点色：three 从 r137 起支持 vec4 color attribute，
    // 最后一个分量直接乘进 opacity —— 这样「关系越强线越实」（需求 #6）
    // 不用给每条边单独建材质，也就不会多出 draw call。
    const lc = new Float32Array(segs * 8);
    let s = 0;
    for (let e = 0; e < nEdge; e++) {
      const l = this.links[e];
      const tracks = this.edgeTracks(l.strength, l.type);
      const dashes = this.edgeDashes(l.type);
      for (let k = 0; k < tracks; k++) {
        const shift = (k - (tracks - 1) / 2) * 0.45;
        for (let d = 0; d < dashes; d++) {
          this.segEdge[s] = e;
          this.segShift[s] = shift;
          this.segTa[s] = d / dashes;
          this.segTb[s] = (d + 0.5) / dashes;
          s++;
        }
      }
    }
    this.linkGeom.setAttribute("position", new THREE.BufferAttribute(this.linkPos, 3));
    this.linkGeom.setAttribute("color", new THREE.BufferAttribute(lc, 4));
    this.linkGeom.setDrawRange(0, segs * 2);
  }

  /** strength → 平行线数量（1 条 = 1px，3 条 ≈ 3px）。 */
  private edgeTracks(strength: number, type: string): number {
    const s = Math.max(0, Math.min(1, strength ?? 0.2));
    const base = s > 0.66 ? 3 : s > 0.33 ? 2 : 1;
    return type === "tag" || type === "similar" ? Math.max(1, base - 1) : base;
  }

  /** 虚线密度：标签/相似关系画成虚线（#22），层级包含关系是实线。 */
  private edgeDashes(type: string): number {
    return type === "tag" || type === "similar" ? 6 : 1;
  }

  /** 节点视觉半径：越大越亮的球（#5/#6 的尺寸语义）。 */
  private radiusOf(node: SNode): number {
    if (node.type === "root") return 26;
    // 恒星（盘符）的半径按**物理容量**算（需求：容量大的恒星要明显更大）。
    // 旧实现按 log2(文件数) 算，结果是"文件多的盘更大" —— 一块 4TB 的
    // 冷备份盘经常文件数很少，缩成一个点，用户完全看不出容量差异。
    if (node.type === "drive") {
      const cap = node.capacity ?? 0;
      if (cap > 0) return starRadiusOf(cap);
      // 拿不到容量时退回文件数估算，至少不会变成 0。
      return 60 + Math.min(30, Math.log2(node.count + 1) * 3.2);
    }
    if (node.type === "kind") return 18 + Math.min(22, Math.log2(node.count + 1) * 3);
    if (node.type === "folder") return 9 + Math.min(16, Math.log2(node.count + 1) * 2.6);
    return 3.4 + Math.min(4, Math.log2(node.size + 2) * 0.5);
  }

  private pushGraph() {
    // 需求 #5/#6 改成恒星级轨道后，物理引擎不该再插手坐标：
    // 真实引力引力积分在展开上百个节点时会掉到个位数帧率，节点还会互相
    // 吞并/甩飞。这里把 d3 的 charge/link 强度全部归零，只保留它的
    // 图结构索引（拾取、邻居、hover 高亮都要用）。
    //
    // 【需求 #4「星星点不开、一点就报错」的真凶就在这里】
    // d3-force 的 forceLink.initialize() 会**就地**把 link.source/target 从
    // id 字符串改写成节点对象；而 three-forcegraph 的 update 顺序是
    // 先 numDimensions() 再 nodes()，前者会拿「上一轮的节点表 + 这一轮的
    // 链接表」初始化 link force。如果链接表和上一轮是同一个数组引用，
    // addData 刚 push 进去的新链接（target 是本轮才加入的节点）就会在旧
    // 节点表里查不到，d3 直接抛 Error: node not found: kind:other@D:
    // 每次给 d3 一份**新的链接副本**，它改写谁都不影响下一轮的初始化。
    this.fg.graphData({ nodes: this.nodes, links: this.graphLinks() });
    this.applyOrbitForces();
    this.syncPositions();
  }

  /**
   * 交给 d3 的链接表：规范化成字符串 id，并丢掉两端不在图里的残边。
   *
   * 每次 pushGraph 都新建一份 —— 理由见 pushGraph 的注释：d3 会把
   * source/target 改写成节点对象，复用同一个数组会让下一轮初始化拿到
   * 已被污染的链接。这里用 idOf 同时兼容「已是对象」的历史数据。
   */
  private graphLinks(): SLink[] {
    const out: SLink[] = [];
    for (const l of this.links) {
      const source = this.idOf(l.source);
      const target = this.idOf(l.target);
      if (!this.byId.has(source) || !this.byId.has(target)) continue;
      out.push({ source, target, type: l.type, strength: l.strength });
    }
    return out;
  }

  /** 把 3D 坐标写进 GPU 缓冲（力导向每次 tick 调用）。 */
  private syncPositions() {
    if (this.disposed) return;
    const off = this.offArr;
    for (let i = 0; i < this.nodes.length; i++) {
      const n = this.nodes[i];
      off[i * 3] = n.x || 0;
      off[i * 3 + 1] = n.y || 0;
      off[i * 3 + 2] = n.z || 0;
    }
    const attr = this.nodeGeom.getAttribute("aOffset") as THREE.InstancedBufferAttribute | undefined;
    if (attr) attr.needsUpdate = true;
    // 恒星坐标必须一起写：它们换了一套缓冲，漏掉的话恒星会停在初始位置不动。
    this.syncStars();

    const lp = this.linkPos;
    const lcol = this.linkGeom.getAttribute("color") as THREE.BufferAttribute | undefined;
    const c = new THREE.Color();
    for (let i = 0; i < this.segCount; i++) {
      const l = this.links[this.segEdge[i]];
      if (!l) continue;
      const a = this.byId.get(this.idOf(l.source));
      const b = this.byId.get(this.idOf(l.target));
      if (!a || !b) continue;
      // 沿 a→b 取两个参数点，再沿屏幕无关的固定垂直方向平移，
      // 多条平行线拼出 1~3px 的视觉线宽（#6）。
      const ta = this.segTa[i];
      const tb = this.segTb[i];
      let dx = b.x - a.x;
      let dy = b.y - a.y;
      let dz = b.z - a.z;
      // 垂线方向：与线段叉乘一个参考轴，退化时换另一个轴。
      let px = -dy;
      let py = dx;
      let pz = 0;
      if (Math.abs(px) + Math.abs(py) < 0.001) {
        px = dz;
        py = 0;
        pz = -dx;
      }
      const pl = Math.hypot(px, py, pz) || 1;
      const sh = this.segShift[i] / pl;
      px *= sh;
      py *= sh;
      pz *= sh;
      lp[i * 6] = a.x + dx * ta + px;
      lp[i * 6 + 1] = a.y + dy * ta + py;
      lp[i * 6 + 2] = a.z + dz * ta + pz;
      lp[i * 6 + 3] = a.x + dx * tb + px;
      lp[i * 6 + 4] = a.y + dy * tb + py;
      lp[i * 6 + 5] = a.z + dz * tb + pz;
      if (lcol) {
        // 高亮时把邻居连线提亮成青色，否则淡蓝紫（#11/#22）。
        const focus = this.selected || this.hovered;
        const hot = !!focus && (a.id === focus.id || b.id === focus.id);
        c.set(hot ? "#66f0ff" : "#8899ff");
        // w 分量 = 关系强度（需求 #6：关系越强，连线越实）。
        const s = Math.max(0, Math.min(1, l.strength ?? 0.2));
        // 0.12（最弱，几乎只是轮廓）~ 0.67（最强，仍保持淡蓝紫基调）
        const w = hot ? 0.9 : 0.12 + s * 0.55;
        lcol.setXYZW(i * 2, c.r, c.g, c.b, w);
        lcol.setXYZW(i * 2 + 1, c.r, c.g, c.b, w);
      }
    }
    const pa = this.linkGeom.getAttribute("position") as THREE.BufferAttribute | undefined;
    if (pa) pa.needsUpdate = true;
    if (lcol) lcol.needsUpdate = true;
  }

  // ------------------------------------------------------------ 交互

  private bindPointer() {
    const el = this.el;
    let downAt = 0;
    let downX = 0;
    let downY = 0;
    let dragMoved = false;

    el.addEventListener("pointerdown", (e) => {
      downAt = performance.now();
      downX = e.clientX;
      downY = e.clientY;
      dragMoved = false;
    });
    el.addEventListener("pointermove", (e) => {
      if (Math.abs(e.clientX - downX) > 4 || Math.abs(e.clientY - downY) > 4) {
        if (!dragMoved) {
          dragMoved = true;
          // 一开始拖就砍掉后处理，松手后 500ms 再恢复（需求 #32）。
          this.setPostSuspended(true);
        }
      }
      this.pickAt(e.clientX, e.clientY);
    });
    el.addEventListener("pointerup", (e) => {
      if (dragMoved) this.setPostSuspended(false);
      const dt = performance.now() - downAt;
      // 拖拽（旋转相机）不应触发选中，否则转个视角就跳详情。
      if (dragMoved && dt > 180) return;
      const hit = this.pickAt(e.clientX, e.clientY);
      if (hit && dt < 420) {
        this.select(hit);
        this.hooks.onActivate?.(hit);
      } else if (!hit && !dragMoved) {
        this.select(null);
      }
    });
    el.addEventListener("dblclick", (e) => {
      const hit = this.pickAt(e.clientX, e.clientY);
      if (hit) this.focusOn(hit);
    });
    el.addEventListener("contextmenu", (e) => {
      const hit = this.pickAt(e.clientX, e.clientY);
      if (!hit) return;
      e.preventDefault();
      this.hooks.onMenu?.(hit, e.clientX, e.clientY);
    });
    el.addEventListener("pointerleave", () => {
      this.setPostSuspended(false);
      if (this.hovered) {
        this.hovered = null;
        this.hooks.onHover?.(null);
      }
    });
  }

  /** 屏幕坐标拾取：CPU 投影，比 Raycaster 对一万个球做射线检测快两个数量级（#32）。 */
  pickAt(clientX: number, clientY: number): SNode | null {
    const rect = this.el.getBoundingClientRect();
    const mx = clientX - rect.left;
    const my = clientY - rect.top;
    const cam = this.camera as THREE.PerspectiveCamera;
    const v = new THREE.Vector3();
    let best: SNode | null = null;
    let bestD = Infinity;
    for (let i = 0; i < this.nodes.length; i++) {
      const n = this.nodes[i];
      v.set(n.x, n.y, n.z).project(cam);
      if (v.z > 1) continue;
      const sx = (v.x * 0.5 + 0.5) * rect.width;
      const sy = (-v.y * 0.5 + 0.5) * rect.height;
      const d = Math.hypot(sx - mx, sy - my);
      if (d < 20 && d < bestD) {
        bestD = d;
        best = n;
      }
    }
    if (best !== this.hovered) {
      this.hovered = best;
      this.hooks.onHover?.(best);
      // 悬停切边：相关连线提亮、其余变淡（#22），只重写顶点色，很便宜。
      this.syncPositions();
    }
    this.el.style.cursor = best ? "pointer" : "grab";
    return best;
  }

  select(node: SNode | null) {
    this.selected = node;
    // 先建邻居集合再上色，否则选中的第一帧会把邻居也压暗。
    this.neighborSet.clear();
    if (node) {
      for (const l of this.links) {
        const a = this.idOf(l.source);
        const b = this.idOf(l.target);
        if (a === node.id) this.neighborSet.add(b);
        else if (b === node.id) this.neighborSet.add(a);
      }
    }
    this.paintAlpha();
    this.hooks.onSelect?.(node);
    this.syncPositions();
  }

  /** 选中时把无关节点压暗（#16 高亮），而不是隐藏 —— 隐藏会让布局跟着跳。 */
  private paintAlpha() {
    for (let i = 0; i < this.nodes.length; i++) {
      const n = this.nodes[i];
      // 图例过滤优先于选中高亮：被筛掉的节点应该彻底消失（#22 标签过滤）。
      if (this.hiddenKinds.size && this.hiddenKinds.has(n.kind)) {
        this.alphaArr[i] = 0;
        continue;
      }
      // 恒星永远保持满亮度：它是空间参照物，压暗后用户会分不清"这个盘叫什么"。
      if (n.type === "drive") {
        this.alphaArr[i] = this.hiddenKinds.has(n.kind) ? 0 : 1;
        continue;
      }
      // 选中高亮优先于聚光：用户主动点谁就聚焦谁。
      if (this.selected) {
        if (n.id === this.selected.id) this.alphaArr[i] = 1;
        else if (this.neighborSet.has(n.id)) this.alphaArr[i] = 0.85;
        else this.alphaArr[i] = 0.16;
      } else if (this.spotId) {
        if (n.id === this.spotId) this.alphaArr[i] = 1;
        else if (n.parent === this.spotId) this.alphaArr[i] = 0.95;
        else this.alphaArr[i] = 0.18;
      } else {
        this.alphaArr[i] = 1;
      }
    }
    const a = this.nodeGeom.getAttribute("aAlpha") as THREE.InstancedBufferAttribute | undefined;
    if (a) a.needsUpdate = true;
  }

  /** 相机聚焦到某个节点（#7 自动聚焦 + #21 钻取动画）。 */
  focusOn(node: SNode | null, ms = 1000) {
    this.focusId = node?.id ?? null;
    if (!node) {
      this.fg.cameraPosition({ x: 0, y: 0, z: this.cameraDistance() }, { x: 0, y: 0, z: 0 }, ms);
      return;
    }
    const dist = Math.max(70, this.radiusOf(node) * 6);
    const hyp = Math.hypot(node.x, node.y, node.z) || 1;
    const ratio = 1 + dist / hyp;
    this.fg.cameraPosition(
      { x: node.x * ratio, y: node.y * ratio, z: node.z * ratio },
      { x: node.x, y: node.y, z: node.z },
      ms,
    );
  }

  cameraDistance(): number {
    const cam = this.camera as THREE.PerspectiveCamera;
    return cam.position.length() || 480;
  }

  /** 复位视角：看全图（#7）。 */
  fit(ms = 800) {
    this.focusId = null;
    this.fg.zoomToFit(ms, 90);
  }

  /**
   * 按恒星的实际分布算一个合适的相机距离。
   *
   * 为什么不用 zoomToFit：3d-force-graph 的 zoomToFit 按**所有可见节点**
   * 的包围球算，而我们的节点是 InstancedBufferGeometry，fg 那边还挂着
   * 一份不可见的节点表（nodeVisibility 全关），包围球不可靠。
   * 恒星只有个位数，直接按它们的坐标与半径手算，结果稳定。
   */
  frameStars(ms = 900) {
    const stars = this.nodes.filter((n) => n.type === "drive");
    if (!stars.length) return;
    // 场景尺度 = 恒星圆心到质心的最大距离 + 最大日冕半径（2.6 倍恒星半径）。
    let cx = 0;
    let cy = 0;
    let cz = 0;
    for (const s of stars) {
      cx += s.x;
      cy += s.y;
      cz += s.z;
    }
    cx /= stars.length;
    cy /= stars.length;
    cz /= stars.length;
    let extent = 0;
    let maxR = 0;
    for (const s of stars) {
      extent = Math.max(extent, Math.hypot(s.x - cx, s.y - cy, s.z - cz));
      maxR = Math.max(maxR, this.radiusOf(s));
    }
    // 半径要包住"最远的恒星 + 它的日冕"，再留 35% 呼吸空间。
    const need = (extent + maxR * 2.6) * 1.35;
    // 透视相机：可视高度 = 2 * dist * tan(fov/2)，fov 默认 50°。
    const fov = ((this.camera as THREE.PerspectiveCamera).fov ?? 50) * (Math.PI / 180);
    const dist = Math.max(220, need / Math.tan(fov / 2));
    this.fg.cameraPosition(
      { x: cx, y: cy + dist * 0.18, z: cz + dist },
      { x: cx, y: cy, z: cz },
      ms,
    );
  }

  zoomBy(factor: number) {
    const cam = this.camera as THREE.PerspectiveCamera;
    const p = cam.position;
    const d = Math.max(0.2, Math.min(5, factor));
    this.fg.cameraPosition({ x: p.x * d, y: p.y * d, z: p.z * d }, { x: 0, y: 0, z: 0 }, 240);
  }

  // ------------------------------------------------------------ LOD / 后处理

  /**
   * 按到相机的距离给每个节点算 LOD 系数（需求 #30）：
   * 近处 1（完整光晕 + 大尺寸），远处趋近 0（只剩亮芯、更小）。
   * 用平滑的 smoothstep 而不是硬切，避免拖动时节点"啪"地换模（#30 平滑过渡）。
   */
  private updateLod() {
    const cam = this.camera as THREE.PerspectiveCamera;
    const cp = cam.position;
    const n = this.nodes.length;
    if (this.lodArr.length !== n) this.lodArr = new Float32Array(n);
    // 以整图尺度做归一化，这样放大缩小时比例仍然合理。
    let far = 1;
    for (let i = 0; i < n; i++) {
      const d = Math.hypot(this.nodes[i].x - cp.x, this.nodes[i].y - cp.y, this.nodes[i].z - cp.z);
      if (d > far) far = d;
    }
    const near = Math.max(30, far * 0.25);
    for (let i = 0; i < n; i++) {
      const nd = this.nodes[i];
      const d = Math.hypot(nd.x - cp.x, nd.y - cp.y, nd.z - cp.z);
      const t = (d - near) / Math.max(1, far - near);
      this.lodArr[i] = 1 - Math.max(0, Math.min(1, t));
    }
    const attr = this.nodeGeom.getAttribute("aLod") as THREE.InstancedBufferAttribute | undefined;
    if (attr) attr.needsUpdate = true;
  }

  /**
   * 交互期临时关闭后处理（需求 #32）：拖拽旋转时帧率优先，
   * 松手 500ms 后恢复完整效果。切换不做淡入淡出，因为 Bloom 开关本身
   * 是逐帧的，硬切在视觉上只是一次亮度变化，比"半透明混合"更省。
   */
  private setPostSuspended(on: boolean) {
    if (this.postSuspended === on) return;
    this.postSuspended = on;
    if (this.bloomPass) this.bloomPass.enabled = !on && !!this.opts.bloom;
    if (this.sketchPass) this.sketchPass.enabled = !on && !!this.opts.sketch;
    if (this.resumeTimer) {
      clearTimeout(this.resumeTimer);
      this.resumeTimer = 0;
    }
    if (on) return;
    this.resumeTimer = window.setTimeout(() => {
      this.resumeTimer = 0;
      if (this.disposed || this.postSuspended) return;
      if (this.bloomPass) this.bloomPass.enabled = !!this.opts.bloom;
      if (this.sketchPass) this.sketchPass.enabled = !!this.opts.sketch;
    }, 500);
  }

  // ------------------------------------------------------------ 循环

  private startLoop() {
    const tick = (t: number) => {
      if (this.disposed) return;
      const dt = this.lastT ? Math.min(0.1, (t - this.lastT) / 1000) : 0.016;
      this.lastT = t;
      this.raf = requestAnimationFrame(tick);

      this.nebula?.update(dt);
      // 轨道推进（需求 #5/#6）：解析式，O(n)，只写 JS 侧坐标，
      // 真正的 GPU 上传交给下面统一的 syncPositions。
      if (this.orbitOrder.length) {
        this.updateOrbits(dt);
        this.syncPositions();
      }
      this.nodeMat.uniforms.uTime.value = t / 1000;
      this.starMat.uniforms.uTime.value = t / 1000;
      // 耀斑缓慢衰减回基线：爆发是"事件"，不该永久亮着。
      if (this.starFlare.length) {
        let dirty = false;
        for (let k = 0; k < this.starFlare.length; k++) {
          if (this.starFlare[k] > 0.25) {
            this.starFlare[k] = Math.max(0.25, this.starFlare[k] - dt * 0.55);
            dirty = true;
          }
        }
        if (dirty) {
          const a = this.starGeom.getAttribute("aFlare") as THREE.InstancedBufferAttribute | undefined;
          if (a) a.needsUpdate = true;
        }
      }
      this.applyDpr();
      if (this.sketchPass?.enabled) this.sketchPass.uniforms.uTime.value = t / 1000;
      // LOD 不必每帧重算：位置在力导向收敛后基本不动，5 次/秒足够（#30）。
      if (t - this.lodTimer > 200) {
        this.lodTimer = t;
        this.updateLod();
      }
      this.hooks.onFrame?.();
      this.checkFps(dt);
    };
    this.raf = requestAnimationFrame(tick);
  }

  /**
   * 帧率看门狗（需求 #31/#33）：连续 2 秒低于 30fps 才算"真的慢"，
   * 单帧抖动（切窗口、GC）不该触发降级。
   */
  private checkFps(dt: number) {
    const fps = 1 / Math.max(dt, 0.001);
    this.frameTimes.push(fps);
    if (this.frameTimes.length > 90) this.frameTimes.shift();
    if (this.frameTimes.length < 60) return;
    const avg = this.frameTimes.reduce((a, b) => a + b, 0) / this.frameTimes.length;
    if (avg < 30) {
      this.slowStreak += dt;
      if (this.slowStreak > 2) {
        this.slowStreak = 0;
        this.opts.onSlowFrame?.(Math.round(avg));
      }
    } else {
      this.slowStreak = 0;
    }
  }

  fps(): number {
    if (!this.frameTimes.length) return 0;
    const avg = this.frameTimes.reduce((a, b) => a + b, 0) / this.frameTimes.length;
    return Math.round(avg);
  }

  // ------------------------------------------------------------ 外部

  resize(w: number, h: number) {
    if (w < 2 || h < 2) return;
    this.fg.width(w).height(h);
    this.bloomPass?.setSize(w, h);
    if (this.sketchPass) {
      const dpr = this.currentDpr();
      this.sketchPass.uniforms.uResolution.value.set(w, h);
      const composer = this.fg.postProcessingComposer?.();
      if (composer && this.opts.sketch) {
        const depth = ensureDepthTexture(composer, w * dpr, h * dpr);
        if (depth) this.sketchPass.uniforms.tDepth.value = depth;
      }
    }
    this.applyDpr();
    this.updateLod();
  }

  setDeepSpace(on: boolean) {
    this.opts.deepSpace = on;
    this.applySceneMode();
  }

  setBloom(on: boolean) {
    this.opts.bloom = on;
    this.applyBloom();
  }

  setSketch(on: boolean) {
    this.opts.sketch = on;
    this.applySketch();
  }

  setDprOverride(v: number) {
    this.opts.dprOverride = v;
    this.applyDpr();
  }

  /** 供标签层取屏幕坐标（需求 #18：只在屏幕上标少量文字）。 */
  projectNodes(ids: string[]) {
    const rect = this.el.getBoundingClientRect();
    const cam = this.camera as THREE.PerspectiveCamera;
    const v = new THREE.Vector3();
    const out = new Map<string, { x: number; y: number; visible: boolean }>();
    for (const id of ids) {
      const n = this.byId.get(id);
      if (!n) continue;
      v.set(n.x, n.y, n.z).project(cam);
      if (v.z > 1) {
        out.set(id, { x: 0, y: 0, visible: false });
        continue;
      }
      out.set(id, {
        x: (v.x * 0.5 + 0.5) * rect.width,
        y: (-v.y * 0.5 + 0.5) * rect.height,
        visible: true,
      });
    }
    return out;
  }

  /**
   * 层级聚光（#19「点击类型节点后平滑展开子分类，旧节点淡出」）：
   * 钻取到某节点后，把它和它刚展开的一级子节点保持在正常亮度，
   * 其余节点压到 0.18 —— 既不删节点（布局不跳），又让视线落在新层级上。
   */
  spotlight(id: string | null) {
    this.spotId = id;
    this.paintAlpha();
  }

  /** 图例过滤（#5/#22）：按节点 kind 显示或隐藏，返回当前隐藏集合。 */
  toggleKind(kind: string): boolean {
    if (this.hiddenKinds.has(kind)) this.hiddenKinds.delete(kind);
    else this.hiddenKinds.add(kind);
    this.paintAlpha();
    this.syncPositions();
    return this.hiddenKinds.has(kind);
  }

  hiddenKindList(): string[] {
    return [...this.hiddenKinds];
  }

  /** 外部改过坐标（布局缓存回灌）后强制刷新 GPU 缓冲。 */
  refreshLayout() {
    this.syncPositions();
    this.paintAlpha();
  }

  nodeById(id: string): SNode | null {
    return this.byId.get(id) ?? null;
  }

  allNodes(): SNode[] {
    return this.nodes;
  }

  /** 需求 #29 的可验证指标：当前 draw call 数（renderer.info）。 */
  drawCalls(): number {
    return this.renderer.info.render.calls;
  }

  /** 让相机随节点位置平滑跟随（聚焦模式下持续对准）。 */
  trackFocus(ms = 300) {
    if (!this.focusId) return;
    const n = this.byId.get(this.focusId);
    if (n) this.focusOn(n, ms);
  }

  dispose() {
    this.disposed = true;
    cancelAnimationFrame(this.raf);
    if (this.resumeTimer) clearTimeout(this.resumeTimer);
    if (typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", this.onVisibility);
    }
    this.nebula?.dispose();
    this.nebula = null;
    this.nodeGeom.dispose();
    this.nodeMat.uniforms.uGlow?.value?.dispose?.();
    this.nodeMat.dispose();
    this.starGeom.dispose();
    this.starMat.dispose();
    this.scene.remove(this.starMesh);
    this.linkGeom.dispose();
    this.linkMat.dispose();
    this.scene.remove(this.nodeMesh);
    this.scene.remove(this.linkLines);
    try {
      this.fg._destructor?.();
    } catch {
      /* 3d-force-graph 的析构在部分版本会抛，吞掉即可 */
    }
  }
}
