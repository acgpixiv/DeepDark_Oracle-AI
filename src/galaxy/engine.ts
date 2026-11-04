// 星系图谱渲染引擎（Canvas 2D + d3-force）。
// 需求 REQ-012~041：力导向布局、三级 LOD、视口裁剪、平滑相机动画（聚焦/复位）、
// 拖拽与缩放交互。这里只碰 Canvas 与模拟，不涉及 Vue 响应式。
import {
  forceSimulation,
  forceLink,
  forceManyBody,
  forceCenter,
  forceCollide,
  type Simulation,
  type SimulationNodeDatum,
} from "d3-force";
import {
  BLUEPRINT_CONFIG,
  BLUEPRINT_FORCE_CANVAS2D,
  FORCE,
  LAYOUT_TRANSITION_MS,
  MAX_ZOOM,
  MIN_ZOOM,
  capsuleFor,
  spiralLayout,
  type GEdge,
  type GNode,
  type LayoutMode,
} from "./model";
import { layoutClusters, layoutRadial, layoutTree } from "./layouts2d";
import {
  QUALITY_PROFILES,
  webglSupport,
  effectiveDpr,
  pickBackend,
  type QualityPreset,
  type QualityProfile,
  type RenderBackend,
  type WebGLSupport,
} from "./capability";
import { GLGalaxyRenderer } from "./glRenderer";
import { recordCrash, watchContextLoss } from "./telemetry";

export interface Camera {
  x: number;
  y: number;
  /** 缩放倍数，1 表示节点半径按世界坐标 1:1 显示。 */
  k: number;
}

export type FocusPhase = "IDLE" | "FOCUSING" | "FOCUSED" | "RESTORING";

export interface EngineHooks {
  /** 左键单击节点（选空）。 */
  onSelect?(node: GNode | null): void;
  /** 双击节点：文件夹=展开/折叠，文件=打开。 */
  onActivate?(node: GNode): void;
  /** 右键菜单。 */
  onMenu?(node: GNode | null, clientX: number, clientY: number): void;
  /** 悬停变化。 */
  onHover?(node: GNode | null): void;
  /** 相机变化，用于刷新缩放比例显示。 */
  onCamera?(camera: Camera): void;
  /** 把一个节点拖到另一个节点上（REQ-074 空间捏合建边）。 */
  onPinchPair?(a: GNode, b: GNode): void;
}

/** 构造参数：画质、后端偏好与慢帧回调（REQ-103 / REQ-108）。 */
export interface EngineOptions {
  /** 画质档位，默认 medium。 */
  quality?: QualityPreset;
  /** 后端偏好：auto 自动探测、webgl 强制 GPU、canvas 强制 2D。 */
  backend?: "auto" | "webgl" | "canvas";
  /** WebGL 模式下主 canvas 被占用，文字标签画在这张覆盖层上。 */
  labelCanvas?: HTMLCanvasElement | null;
  /** 连续掉帧时回调一次，用于提示用户降画质。 */
  onSlowFrame?: (frameMs: number) => void;
  /** 渲染分辨率倍数；0 表示跟随画质自动（REQ-106）。 */
  dprOverride?: number;
}

interface SimNode extends GNode, SimulationNodeDatum {}

const TAU = Math.PI * 2;

/**
 * 圆角矩形路径。
 *
 * 为什么不直接用 ctx.roundRect：它要 Chrome 99+，而 WebView2 的版本跟着系统走，
 * 老机器上会直接抛 TypeError；手写一遍只有十几行，换来的是一致可用。
 */
function roundRectPath(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number
) {
  const rr = Math.max(0, Math.min(r, Math.min(w, h) / 2));
  ctx.beginPath();
  ctx.moveTo(x + rr, y);
  ctx.lineTo(x + w - rr, y);
  ctx.arcTo(x + w, y, x + w, y + rr, rr);
  ctx.lineTo(x + w, y + h - rr);
  ctx.arcTo(x + w, y + h, x + w - rr, y + h, rr);
  ctx.lineTo(x + rr, y + h);
  ctx.arcTo(x, y + h, x, y + h - rr, rr);
  ctx.lineTo(x, y + rr);
  ctx.arcTo(x, y, x + rr, y, rr);
  ctx.closePath();
}

/** 蓝图风的等宽字体栈：标签必须等宽，否则截断后的卡片宽窄抖动。 */
const BLUEPRINT_FONT = '"Courier New",Consolas,"Microsoft YaHei",monospace';

export class GalaxyEngine {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D | null;
  private hooks: EngineHooks;

  /** WebGL 渲染器；为 null 表示走 Canvas 2D 回退管线。 */
  private gl: GLGalaxyRenderer | null = null;
  /** WebGL 模式下的文字覆盖层（主 canvas 已绑定 WebGL 上下文）。 */
  private labelCtx: CanvasRenderingContext2D | null = null;
  private labelCanvas: HTMLCanvasElement | null = null;
  private support: WebGLSupport;
  private profile: QualityProfile;
  private backend: RenderBackend;
  /** 用户偏好，供设置页回读。 */
  private preference: "auto" | "webgl" | "canvas" = "auto";
  /** WebGL 初始化失败时的原因，诊断面板会显示。 */
  fallbackReason = "";
  /** 最近一帧的实际提交耗时（毫秒）。 */
  frameMs = 0;
  private slowFrames = 0;
  private onSlowFrame?: (frameMs: number) => void;
  /** 渲染分辨率倍数覆盖（0 = 自动）。 */
  dprOverride = 0;
  /** WebGL 上下文丢失监听解绑函数。 */
  private unwatchContext: (() => void) | null = null;

  private nodes: SimNode[] = [];
  private edges: GEdge[] = [];
  private sim: Simulation<SimNode, undefined> | null = null;

  private camera: Camera = { x: 0, y: 0, k: 1 };
  private targetCamera: Camera = { x: 0, y: 0, k: 1 };
  private cameraAnimating = false;

  private width = 800;
  private height = 600;
  private dpr = 1;

  private hovered: SimNode | null = null;
  private selected: SimNode | null = null;
  private focus: SimNode | null = null;
  private focusPhase: FocusPhase = "IDLE";
  private neighbors = new Set<string>();

  private dragging: SimNode | null = null;
  private panning = false;
  private pointer = { x: 0, y: 0, down: false, moved: false, lastX: 0, lastY: 0, downX: 0, downY: 0 };

  /** 鱼眼透镜（REQ-077）：开启后鼠标附近节点被放大并向外推开。 */
  fisheye = false;
  fisheyeRadius = 150;
  fisheyeScale = 1.5;

  /** 渲染统计，用于测帧率与裁剪数量（REQ-071 验收）。 */
  stats = { fps: 0, drawn: 0, culled: 0 };

  private raf = 0;
  private lastFrame = 0;
  private lastDrawMs = 0;
  private slowHintFired = false;

  private frameAccum = 0;
  private frameCount = 0;
  private dirty = true;

  /**
   * 当前布局模式（需求「一键整理」）。
   *
   * force 走原来那套 d3 力导向；tree / radial 是**解析布局**：坐标算一次，
   * 然后由布局过渡动画把它们挪过去，之后 d3 完全停摆（等价于给所有节点
   * 打上 fx/fy）。这样切回 force 时把 fx/fy 清空、alpha 重开即可，不用重建。
   */
  private layoutMode: LayoutMode = "force";
  /** 布局过渡动画：从旧坐标到新坐标的插值进度。 */
  private layoutAnim: {
    from: Map<string, { x: number; y: number }>;
    to: Map<string, { x: number; y: number }>;
    startedAt: number;
  } | null = null;
  /** 主循环里推动画用的回调（没有过渡时为空）。 */
  private layoutFrame: (() => void) | null = null;

  /**
   * 语义簇元信息（"排序模式"用）：簇 id、名字、颜色、成员数。
   *
   * 画簇包络需要它。为什么不让引擎自己从节点的 clusterId 推：
   * 簇名是后端根据簇内代表性文件名算的（"以 xxx 为主"），前端推不出来。
   */
  private clusterMeta: Array<{ id: number; label: string; color: string; size: number }> = [];

  constructor(canvas: HTMLCanvasElement, hooks: EngineHooks = {}, opts: EngineOptions = {}) {
    this.canvas = canvas;
    this.hooks = hooks;
    this.preference = opts.backend || "auto";
    this.profile = QUALITY_PROFILES[opts.quality || "medium"] || QUALITY_PROFILES.medium;
    this.onSlowFrame = opts.onSlowFrame;
    this.dprOverride = opts.dprOverride || 0;
    this.support = webglSupport();

    let backend = pickBackend(this.support, this.preference);
    // 蓝图风强制走 Canvas 2D：GL 管线只能画「发光圆点」，补不出圆角卡片与虚线，
    // 硬改等于重写渲染器。原 GL 代码与探测结果都保留，只是不选中它。
    if (BLUEPRINT_FORCE_CANVAS2D) backend = "canvas2d";
    if (backend !== "canvas2d") {
      try {
        this.gl = new GLGalaxyRenderer(canvas, this.support, this.profile);
      } catch (err) {
        // 着色器编译失败 / 驱动拒绝创建上下文：退回 Canvas 2D。
        this.gl = null;
        this.fallbackReason = "WebGL 初始化失败：" + String(err);
        backend = "canvas2d";
        recordCrash({
          kind: "init-fail",
          backend: this.support.level,
          renderer: this.support.renderer || "unknown",
          detail: this.fallbackReason,
        });
      }
    }
    this.backend = backend;

    if (backend === "canvas2d") {
      const ctx = canvas.getContext("2d", { alpha: true });
      if (!ctx) {
        // 极端情况：canvas 已被 WebGL 占用且 2D 拿不到。交给上层换画布重建。
        throw new Error(this.fallbackReason || "Canvas 2D 不可用");
      }
      this.ctx = ctx;
    } else {
      // WebGL 分支同样准备一个 2D 上下文对象，标签层用它；主 canvas 不能再用 2D。
      this.ctx = canvas.getContext("2d") as unknown as CanvasRenderingContext2D;
      const lc = opts.labelCanvas || null;
      this.labelCanvas = lc;
      if (lc) this.labelCtx = lc.getContext("2d", { alpha: true });
    }

    // REQ-108：上下文丢失/恢复必须留证，否则用户只会看到"图谱白屏了"。
    this.unwatchContext = watchContextLoss(canvas, () => ({
      backend: this.backend,
      renderer: this.support.renderer || "unknown",
    }));

    this.bindEvents();
    this.loop();
  }

  /** 当前实际后端（诊断面板显示）。 */
  get renderBackend(): RenderBackend {
    return this.backend;
  }

  /** WebGL 探测结果（显卡型号 / 驱动补丁）。 */
  get glSupport(): WebGLSupport {
    return this.support;
  }

  /** 当前画质档位。 */
  get qualityKey(): QualityPreset {
    return this.profile.key;
  }

  /** 切换画质：WebGL 下重建程序，Canvas 下改变 LOD 上限。 */
  setQuality(key: QualityPreset) {
    const prof = QUALITY_PROFILES[key];
    if (!prof || prof === this.profile) return;
    this.profile = prof;
    this.gl?.setProfile(prof);
    // 换了画质就重新观察一次帧预算，否则一次卡顿会永久压制后续提示。
    this.slowFrames = 0;
    this.slowHintFired = false;
    // 画质档位会改变 devicePixelRatio 上限（REQ-106），必须重设画布像素尺寸。
    this.resize(this.width, this.height);
    this.dirty = true;
  }

  /** 切换渲染分辨率倍数（0 = 自动），立即生效。 */
  setDprOverride(value: number) {
    const next = Math.max(0, Math.min(3, Math.round(value || 0)));
    if (next === this.dprOverride) return;
    this.dprOverride = next;
    this.resize(this.width, this.height);
  }

  /** 实际生效的着色器精度（highp / mediump），兼容性诊断用（REQ-104）。 */
  get shaderPrecision(): string {
    return this.gl ? this.gl.shaderPrecision : "n/a（Canvas 2D）";
  }

  /** 命中并已应用的驱动兼容补丁（REQ-105）。 */
  get appliedPatches() {
    return this.support.patches;
  }

  /** 运行期切换后端偏好；需要重建引擎才能生效，返回是否发生了变化。 */
  setBackendPreference(pref: "auto" | "webgl" | "canvas"): boolean {
    let next = pickBackend(this.support, pref === "webgl" ? "webgl" : pref === "canvas" ? "canvas" : "auto");
    // 蓝图风下任何偏好都会落回 Canvas 2D。不在这里也强制一次，
    // 用户在设置里切成 WebGL 会白白重建一次引擎（换画布、重新布局）却看不到任何变化。
    if (BLUEPRINT_FORCE_CANVAS2D) next = "canvas2d";
    const changed = next !== this.backend;
    this.preference = pref;
    return changed;
  }

  // ---------------------------------------------------------- 生命周期

  resize(w: number, h: number, dpr = window.devicePixelRatio || 1) {
    this.width = Math.max(1, Math.floor(w));
    this.height = Math.max(1, Math.floor(h));
    // REQ-106：dpr 由画质档位与后端决定（低端/Canvas 路径封顶，避免 3 倍像素白烧）。
    this.dpr = effectiveDpr(this.profile, this.backend, dpr, this.dprOverride);
    const pw = Math.floor(this.width * this.dpr);
    const ph = Math.floor(this.height * this.dpr);
    if (this.gl) {
      this.gl.resize(this.width, this.height, this.dpr);
    } else {
      this.canvas.width = pw;
      this.canvas.height = ph;
    }
    this.canvas.style.width = this.width + "px";
    this.canvas.style.height = this.height + "px";
    const lc = this.labelCanvas;
    if (lc) {
      lc.width = pw;
      lc.height = ph;
      lc.style.width = this.width + "px";
      lc.style.height = this.height + "px";
    }
    this.dirty = true;
  }

  destroy() {
    cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.sim?.stop();
    this.sim = null;
    this.unbindEvents();
    this.unwatchContext?.();
    this.unwatchContext = null;
    this.gl?.destroy();
    this.gl = null;
  }

  // ---------------------------------------------------------- 数据

  /** 全量替换节点与边（首屏 / 换根时调用）。 */
  setData(nodes: GNode[], edges: GEdge[]) {
    this.sim?.stop();
    const sim: SimNode[] = nodes.map((n) => ({ ...n }));
    const byId = new Map<string, SimNode>();
    for (const n of sim) byId.set(n.id, n);

    const links: any[] = [];
    for (const e of edges) {
      const s = byId.get(e.source.id);
      const t = byId.get(e.target.id);
      if (!s || !t) continue;
      links.push({ source: s, target: t, type: e.type, strength: e.strength });
    }

    spiralLayout(sim as unknown as GNode[]);
    this.nodes = sim;
    this.edges = links.map((l) => ({
      source: l.source as GNode,
      target: l.target as GNode,
      type: l.type,
      strength: l.strength,
    }));

    const simu = forceSimulation<SimNode>(sim)
      .alpha(FORCE.alpha)
      .alphaDecay(FORCE.alphaDecay)
      .velocityDecay(FORCE.velocityDecay)
      .force(
        "link",
        forceLink<SimNode, any>(links)
          .id((d: any) => d.id)
          .distance((l: any) => FORCE.linkDistance * (l.target && l.target.type === "file" ? 0.72 : 1))
          .strength((l: any) => FORCE.linkStrength * (0.6 + 0.4 * (l.strength || 1)))
      )
      .force("charge", forceManyBody<SimNode>().strength(FORCE.charge).distanceMax(520))
      .force("center", forceCenter<SimNode>(0, 0).strength(0.06))
      .force(
        "collide",
        forceCollide<SimNode>((d) => {
          // 卡片是横向长方形：仍按圆点半径做碰撞，左右两侧的卡片会直接压在一起。
          // 用宽高的几何平均近似椭圆，横向拉开、纵向又不至于散架。
          const cap = capsuleFor(d.name || "", d.r || 8);
          return Math.sqrt(cap.halfW * cap.halfH) * FORCE.collide * 0.72;
        }).iterations(1)
      );

    simu.on("tick", () => {
      this.dirty = true;
    });
    this.sim = simu as unknown as Simulation<SimNode, undefined>;

    this.selected = null;
    this.hovered = null;
    this.focus = null;
    this.focusPhase = "IDLE";
    this.neighbors.clear();
    // 新数据进来时，若当前不在力导向模式，直接按解析布局落位。
    if (this.layoutMode !== "force") {
      this.setLayout(this.layoutMode, false);
      return;
    }
    this.fit(true);
    this.dirty = true;
  }

  /** 增量追加（展开目录时用），保留已有节点位置。 */
  addData(nodes: GNode[], edges: GEdge[], parentId?: string) {
    if (!this.sim) return;
    const exist = new Set(this.nodes.map((n) => n.id));
    const fresh: SimNode[] = [];
    for (const n of nodes) {
      if (exist.has(n.id)) continue;
      const parent = parentId ? this.nodes.find((x) => x.id === parentId) : null;
      const s: SimNode = { ...n };
      if (parent) {
        const a = Math.random() * TAU;
        const rad = 30 + Math.random() * 40;
        s.x = (parent.x || 0) + Math.cos(a) * rad;
        s.y = (parent.y || 0) + Math.sin(a) * rad;
      }
      fresh.push(s);
      exist.add(n.id);
    }
    if (!fresh.length) return;

    const byId = new Map<string, SimNode>();
    for (const n of this.nodes) byId.set(n.id, n);
    for (const n of fresh) byId.set(n.id, n);

    const linkData: any[] = [];
    for (const e of this.edges) {
      const s = byId.get(e.source.id);
      const t = byId.get(e.target.id);
      if (s && t) linkData.push({ source: s, target: t, type: e.type, strength: e.strength });
    }
    for (const e of edges) {
      const s = byId.get(e.source.id);
      const t = byId.get(e.target.id);
      if (!s || !t) continue;
      if (linkData.some((l) => l.source === s && l.target === t)) continue;
      linkData.push({ source: s, target: t, type: e.type, strength: e.strength });
    }

    this.nodes = this.nodes.concat(fresh);
    this.edges = linkData.map((l) => ({
      source: l.source as GNode,
      target: l.target as GNode,
      type: l.type,
      strength: l.strength,
    }));

    // 解析布局下，新节点直接归位；力导向下才交给模拟。
    if (this.layoutMode !== "force") {
      this.placeNewNodes(this.layoutMode);
      return;
    }
    this.sim.nodes(this.nodes as any);
    const linkForce = this.sim.force("link") as any;
    if (linkForce) linkForce.links(linkData);
    this.sim.alpha(0.45).restart();
    this.dirty = true;
  }

  /** 折叠：隐藏该目录下所有后代节点。 */
  collapse(nodeId: string) {
    const kids = this.descendantsOf(nodeId);
    for (const n of this.nodes) {
      if (n.id !== nodeId && kids.has(n.id)) n.hidden = true;
    }
    this.sim?.alpha(0.3).restart();
    this.dirty = true;
  }

  /** 展开：把隐藏的后代重新显示。 */
  expand(nodeId: string) {
    const kids = this.descendantsOf(nodeId);
    for (const n of this.nodes) if (kids.has(n.id)) n.hidden = false;
    this.sim?.alpha(0.3).restart();
    this.dirty = true;
  }

  /** 由 contains 边向下走，收集所有后代 id（含自身）。 */
  private descendantsOf(nodeId: string): Set<string> {
    const kids = new Set<string>([nodeId]);
    let grew = true;
    while (grew) {
      grew = false;
      for (const e of this.edges) {
        const s = (e.source as any).id as string;
        const t = (e.target as any).id as string;
        if (kids.has(s) && !kids.has(t)) {
          kids.add(t);
          grew = true;
        }
      }
    }
    return kids;
  }

  /**
   * 按 id 集合显示/隐藏节点（语义按簇筛选用）。
   *
   * 与 `hiddenKinds` 的区别：那个按文件类型过滤（全局、跨数据源），
   * 这个是"只留这些 id"，语义图的分簇筛选需要它。
   * 不删节点、只改 hidden 标志 —— 布局不会跳，恢复时位置还在。
   */
  setVisibleIds(ids: Set<string> | null) {
    for (const n of this.nodes) {
      n.hidden = ids ? !ids.has(n.id) : false;
    }
    this.dirty = true;
  }

  hasVisibleChild(nodeId: string): boolean {
    for (const e of this.edges) {
      const s = (e.source as any).id as string;
      const t = (e.target as any).id as string;
      if (s !== nodeId) continue;
      const child = this.nodes.find((n) => n.id === t);
      if (child && !child.hidden) return true;
    }
    return false;
  }

  // ---------------------------------------------------------- 布局（一键整理）

  /** 当前布局模式。 */
  get layout(): LayoutMode {
    return this.layoutMode;
  }

  /** 数据源是否支持层级布局（语义图没有父子关系，只有簇）。 */
  supportsHierarchy(): boolean {
    return this.nodes.some((n) => !!(n as any).parentId);
  }

  /**
   * 切换布局（需求 #7「力导向 ↔ 层级布局一键切换」）。
   *
   * 为什么要过渡而不是直接跳：节点瞬移会让人瞬间丢失"我是从哪来的"，
   * 尤其是几百个卡片同时换位置 —— 眼睛跟不上，只会觉得画面闪了一下。
   * 800ms 的 ease-out 让用户能看清节点"飞"去哪，建立新的空间记忆。
   *
   * @param mode 目标布局
   * @param animate false 用于首屏/数据重建后直接落位
   */
  /**
   * 设置语义簇元信息（切换数据源时由外部调用）。
   *
   * 两个副作用都要做：
   *   1. 重画（dirty）—— 包络圆与簇名是新东西；
   *   2. **重算布局** —— 若当前已是簇团布局，簇的分组刚变，坐标必须跟着重排。
   *      只置 dirty 不动坐标，画面会停在"没有分组的力导向位置"上，
   *      包络圆算不出来（成员坐标散在全图），看起来就是"什么都没发生"。
   */
  setClusters(list: Array<{ id: number; label: string; color: string; size: number }>) {
    this.clusterMeta = list ?? [];
    this.dirty = true;
    if (this.clusterMeta.length && this.layoutMode === "cluster") {
      // 用非动画方式立即落位：调用点刚设完数据，用户还没看到旧状态。
      this.setLayout("cluster", false);
    }
  }

  setLayout(mode: LayoutMode, animate = true) {
    // 同一个模式 + 要动画 = 用户没换布局，什么也不用做。
    // 但 animate=false 表示"调用方知道数据变了、要求强制重算"，不能提前返回
    // （切换数据源时 layoutMode 已经是 cluster，提前返回会让新分组永远不落位）。
    if (mode === this.layoutMode && animate) return;
    this.layoutMode = mode;
    const targets = this.computeLayout(mode);
    if (!targets.size) return;

    // 记录起点，动画在 loop 里按 ease-out 插值。
    const from = new Map<string, { x: number; y: number }>();
    for (const n of this.nodes) {
      if (!targets.has(n.id)) continue;
      from.set(n.id, { x: n.x, y: n.y });
    }

    if (!animate) {
      for (const n of this.nodes) {
        const t = targets.get(n.id);
        if (!t) continue;
        n.x = t.x;
        n.y = t.y;
        n.vx = 0;
        n.vy = 0;
        // 解析布局下必须锁死坐标，否则 d3 会立刻把它们拽回力导向位置。
        n.fx = t.x;
        n.fy = t.y;
      }
      this.sim?.alpha(0).stop();
      this.layoutAnim = null;
      this.layoutFrame = null;
      this.fit(true);
      this.dirty = true;
      return;
    }

    this.layoutAnim = { from, to: targets, startedAt: performance.now() };
    // 过渡期间让 d3 停摆：它每 tick 都改坐标，会和插值打架。
    this.sim?.alpha(0).stop();
    this.layoutFrame = () => this.stepLayoutAnim();
    this.dirty = true;
  }

  /** 按当前模式算一遍目标坐标（世界坐标）。 */
  private computeLayout(mode: LayoutMode): Map<string, { x: number; y: number }> {
    const vis = this.nodes.filter((n) => !n.hidden);
    if (!vis.length) return new Map();
    if (mode === "tree") return layoutTree(vis);
    if (mode === "radial") return layoutRadial(vis);
    if (mode === "cluster") {
      // 语义簇布局：节点本身没有父子关系，靠外部注入的 clusterId 分组。
      const groups = new Map<number, GNode[]>();
      for (const n of vis) {
        const cid = (n as any).clusterId as number | undefined;
        if (cid === undefined) continue;
        const arr = groups.get(cid);
        if (arr) arr.push(n);
        else groups.set(cid, [n]);
      }
      if (!groups.size) return new Map();
      // 把视口宽高比传进去，网格才能按屏幕形状铺（宽屏多列少行）。
      return layoutClusters(groups, { aspect: this.width / Math.max(1, this.height) });
    }
    return new Map();
  }

  /**
   * 布局过渡的一帧（ease-out quad）。
   *
   * 用 ease-out 而不是 ease-in-out：动画的主体是"落到新位置"，
   * 起步就该有明显位移，越接近终点越慢 —— 这样用户第一眼就知道
   * "正在整理"，而不是盯着静止画面等了半秒才动。
   */
  private stepLayoutAnim() {
    const a = this.layoutAnim;
    if (!a) {
      this.layoutFrame = null;
      return;
    }
    const t = Math.min(1, (performance.now() - a.startedAt) / LAYOUT_TRANSITION_MS);
    // UI 上不用 ease-in（animate 规则的硬要求），这里用 ease-out 幂函数。
    const p = 1 - (1 - t) * (1 - t);
    for (const n of this.nodes) {
      const to = a.to.get(n.id);
      if (!to) continue;
      const from = a.from.get(n.id) ?? to;
      n.x = from.x + (to.x - from.x) * p;
      n.y = from.y + (to.y - from.y) * p;
      n.fx = n.x;
      n.fy = n.y;
    }
    this.dirty = true;
    if (t >= 1) {
      this.layoutAnim = null;
      this.layoutFrame = null;
      // 落位后把相机对准新布局，否则新布局可能整个跑到视口外。
      this.fit(false);
    }
  }

  /** 解析布局下新展开的节点也要立刻归位（不用等整图重排）。 */
  private placeNewNodes(mode: LayoutMode) {
    if (mode === "force") return;
    const targets = this.computeLayout(mode);
    for (const n of this.nodes) {
      const t = targets.get(n.id);
      if (!t || Number.isFinite(n.fx === null ? undefined : n.fx)) {
        // 已经有固定位置的旧节点不动，避免用户正在看的部分跳走。
        if (t && n.fx !== null && n.fx !== undefined) continue;
      }
      if (!t) continue;
      n.x = t.x;
      n.y = t.y;
      n.fx = t.x;
      n.fy = t.y;
    }
    this.dirty = true;
  }

  // ---------------------------------------------------------- 相机

  /** 自适应铺满视口。 */
  fit(instant = false) {
    const vis = this.nodes.filter((n) => !n.hidden);
    if (!vis.length) return;
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of vis) {
      if (!Number.isFinite(n.x) || !Number.isFinite(n.y)) continue;
      minX = Math.min(minX, n.x - n.r);
      minY = Math.min(minY, n.y - n.r);
      maxX = Math.max(maxX, n.x + n.r);
      maxY = Math.max(maxY, n.y + n.r);
    }
    if (!Number.isFinite(minX)) return;

    // 簇团布局要多留两样东西的空间：
    //   1. 包络圆把每个簇撑到"成员最远点 + 56"，节点包围盒并不等于图的实际边界；
    //   2. 簇名画在包络圆上方（再高 ~1.55 个字号），最容易飞出视口。
    // 之前只按节点包围盒 + 60 算，实测缩放 139%，簇名直接被挤出画布外。
    // 这里用"簇成员的外接半径"重算一次边界，并给顶部单独多留 80。
    let topPad = 0;
    if (this.clusterMeta.length && this.layoutMode === "cluster") {
      const byCluster = new Map<number, SimNode[]>();
      for (const n of vis) {
        const cid = (n as any).clusterId as number | undefined;
        if (cid === undefined) continue;
        const arr = byCluster.get(cid);
        if (arr) arr.push(n);
        else byCluster.set(cid, [n]);
      }
      for (const members of byCluster.values()) {
        if (members.length < 2) continue;
        let cx = 0;
        let cy = 0;
        for (const n of members) {
          cx += n.x;
          cy += n.y;
        }
        cx /= members.length;
        cy /= members.length;
        let r = 0;
        for (const n of members) {
          r = Math.max(r, Math.hypot(n.x - cx, n.y - cy));
        }
        // +56 与 drawClusterHulls 里的半径余量一致；簇名已挪进圆内，
        // 不再需要额外顶部留白。
        // +56：点径与描边余量；+34：圆下方那行簇名的高度（下移后必须计入，
        // 否则标签会被画布底边裁掉 —— 上一版把标签放在圆上方时漏算过顶部空间）。
        const hullR = r + 56 + 34;
        minX = Math.min(minX, cx - hullR);
        maxX = Math.max(maxX, cx + hullR);
        minY = Math.min(minY, cy - hullR);
        maxY = Math.max(maxY, cy + hullR);
        topPad = 0;
      }
    }

    const pad = 72;
    const w = Math.max(1, maxX - minX);
    const h = Math.max(1, maxY - minY);
    // 诊断用：把这次 fit 的原始边界挂到实例上，便于在真 GUI 里读。
    // 排障用快照：CDP 里读 window.__ddoEngine._fitDebug 可看到本次 fit
    // 用的边界与实际布局模式，"图不见了"时第一个该查的就是它。
    (this as any)._fitDebug = {
      minX: Math.round(minX), maxX: Math.round(maxX),
      minY: Math.round(minY), maxY: Math.round(maxY),
      w: Math.round(w), h: Math.round(h),
      vw: this.width, vh: this.height,
      layout: this.layoutMode,
      hulls: this.clusterMeta.length,
    };
    // 缩放上限：目录图 1.6（卡片放大一点更好读），点云 1.0。
    // 点云图本身只有 1000px 见方，若沿用 1.6 会被放大到 400%+，
    // 视口里只剩几十个点、其余全在画外 —— 实测正是这个现象。
    const maxK = this.layoutMode === "cluster" ? 1.0 : 1.6;
    const k = Math.max(
      MIN_ZOOM,
      Math.min(
        maxK,
        Math.min(
          (this.width - pad * 2) / w,
          // 顶部给簇名留位置：可用高度相应减少。
          (this.height - pad * 2 - topPad) / h
        )
      )
    );
    // 相机中心上移 topPad 的一半，让多留出来的顶部空间真的落在视野里。
    this.targetCamera = { x: (minX + maxX) / 2, y: (minY + maxY) / 2 - topPad / 2 / k, k };
    if (instant) {
      this.camera = { ...this.targetCamera };
      this.cameraAnimating = false;
      this.hooks.onCamera?.(this.camera);
      this.dirty = true;
    } else {
      this.cameraAnimating = true;
    }
  }

  zoomBy(factor: number, cx?: number, cy?: number) {
    const px = cx === undefined ? this.width / 2 : cx;
    const py = cy === undefined ? this.height / 2 : cy;
    const before = this.worldAt(px, py);
    const k = Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, this.camera.k * factor));
    this.camera.k = k;
    this.camera.x = before.x - (px - this.width / 2) / k;
    this.camera.y = before.y - (py - this.height / 2) / k;
    this.targetCamera = { ...this.camera };
    this.cameraAnimating = false;
    this.hooks.onCamera?.(this.camera);
    this.dirty = true;
  }

  setZoom(k: number) {
    this.zoomBy(k / this.camera.k);
  }

  get zoom(): number {
    return this.camera.k;
  }

  worldAt(px: number, py: number) {
    return {
      x: this.camera.x + (px - this.width / 2) / this.camera.k,
      y: this.camera.y + (py - this.height / 2) / this.camera.k,
    };
  }

  screenOf(n: { x: number; y: number }) {
    return {
      x: (n.x - this.camera.x) * this.camera.k + this.width / 2,
      y: (n.y - this.camera.y) * this.camera.k + this.height / 2,
    };
  }

  /** REQ-017：聚焦状态机 IDLE → FOCUSING → FOCUSED → RESTORING。 */
  focusOn(node: GNode | null) {
    if (!node) {
      this.focusPhase = "RESTORING";
      this.focus = null;
      this.neighbors.clear();
      this.fit(false);
      this.dirty = true;
      return;
    }
    const n = this.nodes.find((x) => x.id === node.id);
    if (!n) return;
    this.focus = n;
    this.focusPhase = "FOCUSING";
    this.neighbors = this.neighborsOf(n.id);
    this.targetCamera = { x: n.x, y: n.y, k: Math.max(1.5, Math.min(MAX_ZOOM, this.camera.k)) };
    this.cameraAnimating = true;
    this.dirty = true;
  }

  get focusNode(): GNode | null {
    return this.focus;
  }

  get focusState(): FocusPhase {
    return this.focusPhase;
  }

  select(node: GNode | null) {
    this.selected = node ? this.nodes.find((n) => n.id === node.id) || null : null;
    this.dirty = true;
  }

  /** 当前参与绘制的全部节点（HUD 统计与布局计算都要用）。 */
  allNodes(): GNode[] {
    return this.nodes;
  }

  get selectedNode(): GNode | null {
    return this.selected;
  }

  /** 一跳邻居（用于高亮关系）。 */
  neighborsOf(id: string): Set<string> {
    const out = new Set<string>();
    for (const e of this.edges) {
      const s = (e.source as any).id as string;
      const t = (e.target as any).id as string;
      if (s === id) out.add(t);
      else if (t === id) out.add(s);
    }
    return out;
  }

  // ---------------------------------------------------------- 交互

  private bindEvents() {
    const c = this.canvas;
    c.addEventListener("pointerdown", this.onPointerDown);
    c.addEventListener("pointermove", this.onPointerMove);
    c.addEventListener("pointerup", this.onPointerUp);
    c.addEventListener("pointercancel", this.onPointerUp);
    c.addEventListener("pointerleave", this.onPointerLeave);
    c.addEventListener("wheel", this.onWheel, { passive: false });
    c.addEventListener("dblclick", this.onDblClick);
    c.addEventListener("contextmenu", this.onContextMenu);
  }

  private unbindEvents() {
    const c = this.canvas;
    c.removeEventListener("pointerdown", this.onPointerDown);
    c.removeEventListener("pointermove", this.onPointerMove);
    c.removeEventListener("pointerup", this.onPointerUp);
    c.removeEventListener("pointercancel", this.onPointerUp);
    c.removeEventListener("pointerleave", this.onPointerLeave);
    c.removeEventListener("wheel", this.onWheel);
    c.removeEventListener("dblclick", this.onDblClick);
    c.removeEventListener("contextmenu", this.onContextMenu);
  }

  private localPoint(e: PointerEvent | WheelEvent | MouseEvent) {
    const r = this.canvas.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  }

  /** 屏幕坐标拾取；离鼠标最近且在半径内者优先。 */
  pick(px: number, py: number): SimNode | null {
    const world = this.worldAt(px, py);
    let best: SimNode | null = null;
    let bestScore = Infinity;
    const k = this.camera.k;
    for (const n of this.nodes) {
      if (n.hidden) continue;
      if (!this.lodVisible(n)) continue;
      const dx = n.x - world.x;
      const dy = n.y - world.y;
      // 卡片是长方形，用圆命中会让「看得见的角落点不中」。
      // 尺寸必须与 draw2D 用同一份 capsuleFor 估算，否则又对不上。
      // cap 是屏幕像素，除以 k 换成世界坐标才能和 dx/dy 比较。
      const cap = capsuleFor(n.name || "", Math.max(1.2, n.r * k));
      const halfW = Math.max(cap.halfW / k, 6 / k);
      const halfH = Math.max(cap.halfH / k, 6 / k);
      if (Math.abs(dx) > halfW || Math.abs(dy) > halfH) continue;
      // 归一化距离：仍在卡片中央附近者优先，避免大卡片抢走小卡片的点击。
      const score = (dx * dx) / (halfW * halfW) + (dy * dy) / (halfH * halfH);
      if (score < bestScore) {
        bestScore = score;
        best = n;
      }
    }
    return best;
  }

  private onPointerDown = (e: PointerEvent) => {
    const p = this.localPoint(e);
    try {
      this.canvas.setPointerCapture(e.pointerId);
    } catch {
      /* 某些合成事件没有可捕获的指针 */
    }
    this.pointer.down = true;
    this.pointer.moved = false;
    this.pointer.downX = p.x;
    this.pointer.downY = p.y;
    this.pointer.lastX = p.x;
    this.pointer.lastY = p.y;
    const hit = this.pick(p.x, p.y);
    if (hit) {
      if (e.button === 0) {
        this.dragging = hit;
        this.sim?.alphaTarget(0.16).restart();
        hit.fx = hit.x;
        hit.fy = hit.y;
      }
    } else {
      this.panning = true;
      this.canvas.style.cursor = "grabbing";
    }
  };

  private onPointerMove = (e: PointerEvent) => {
    const p = this.localPoint(e);
    this.pointer.x = p.x;
    this.pointer.y = p.y;
    if (Math.abs(p.x - this.pointer.downX) > 3 || Math.abs(p.y - this.pointer.downY) > 3) {
      this.pointer.moved = true;
    }

    if (this.dragging) {
      const w = this.worldAt(p.x, p.y);
      // REQ-018：拖拽阻尼 0.85，跟手但保留一点惯性
      this.dragging.x = this.dragging.x + (w.x - this.dragging.x) * (1 - FORCE.dragDamping);
      this.dragging.y = this.dragging.y + (w.y - this.dragging.y) * (1 - FORCE.dragDamping);
      this.dragging.fx = this.dragging.x;
      this.dragging.fy = this.dragging.y;
      this.dirty = true;
      return;
    }
    if (this.panning && this.pointer.down) {
      const dx = p.x - this.pointer.lastX;
      const dy = p.y - this.pointer.lastY;
      this.camera.x -= dx / this.camera.k;
      this.camera.y -= dy / this.camera.k;
      this.targetCamera = { ...this.camera };
      this.pointer.lastX = p.x;
      this.pointer.lastY = p.y;
      this.hooks.onCamera?.(this.camera);
      this.dirty = true;
      return;
    }
    this.pointer.lastX = p.x;
    this.pointer.lastY = p.y;

    const hit = this.pick(p.x, p.y);
    if (hit !== this.hovered) {
      this.hovered = hit;
      this.canvas.style.cursor = hit ? "pointer" : "grab";
      this.hooks.onHover?.(hit);
      this.dirty = true;
    }
    if (this.fisheye) this.dirty = true;
  };

  private onPointerUp = (e: PointerEvent) => {
    try {
      this.canvas.releasePointerCapture(e.pointerId);
    } catch {
      /* ignore */
    }
    const wasDragging = this.dragging;
    const moved = this.pointer.moved;
    if (wasDragging) {
      wasDragging.fx = null;
      wasDragging.fy = null;
      this.sim?.alphaTarget(0);
      if (moved) {
        const p = this.localPoint(e);
        const over = this.pick(p.x, p.y);
        if (over && over.id !== wasDragging.id) this.hooks.onPinchPair?.(wasDragging, over);
      }
    }
    if (!moved) {
      const p = this.localPoint(e);
      const hit = this.pick(p.x, p.y);
      this.select(hit);
      this.hooks.onSelect?.(hit);
      if (!hit && (this.focusPhase === "FOCUSED" || this.focusPhase === "FOCUSING")) {
        this.focusOn(null);
      }
    }
    this.dragging = null;
    this.panning = false;
    this.pointer.down = false;
    this.canvas.style.cursor = "grab";
    this.dirty = true;
  };

  private onPointerLeave = () => {
    if (this.hovered) {
      this.hovered = null;
      this.hooks.onHover?.(null);
      this.dirty = true;
    }
    this.pointer.down = false;
    this.panning = false;
  };

  private onWheel = (e: WheelEvent) => {
    e.preventDefault();
    const p = this.localPoint(e);
    if (e.shiftKey) {
      this.camera.x += (e.deltaY || e.deltaX) / this.camera.k;
      this.targetCamera = { ...this.camera };
      this.hooks.onCamera?.(this.camera);
      this.dirty = true;
      return;
    }
    const factor = Math.exp(-e.deltaY * 0.0016);
    this.zoomBy(factor, p.x, p.y);
  };

  private onDblClick = (e: MouseEvent) => {
    const p = this.localPoint(e);
    const hit = this.pick(p.x, p.y);
    if (hit) this.hooks.onActivate?.(hit);
  };

  private onContextMenu = (e: MouseEvent) => {
    e.preventDefault();
    const p = this.localPoint(e);
    const hit = this.pick(p.x, p.y);
    if (hit) this.select(hit);
    this.hooks.onMenu?.(hit, e.clientX, e.clientY);
  };

  // ---------------------------------------------------------- LOD 与绘制

  /** REQ-032：三级 LOD —— 远看聚合、中看文件夹、近看全部。 */
  private lodVisible(n: GNode): boolean {
    // 语义模式（簇团布局）**一律全显**。
    //
    // 【踩坑】原来的 LOD 是给"目录树图"设计的：k<1 时只显示 folder，
    // 因为远景下几百个文件卡片会糊成一片。但语义图的节点**全是 file**，
    // 一旦切到簇团布局（整图 fit 后 k 常落在 0.4~0.7），就会被这条规则
    // 全部过滤掉 —— 画布上一片空白，HUD 还显示"253 节点"。实测卡了很久。
    if (this.layoutMode === "cluster") return true;
    const k = this.camera.k;
    if (k < 0.3) return n.type === "folder";
    if (k < 1) return n.type === "folder";
    return true;
  }

  private loop = () => {
    this.raf = requestAnimationFrame(this.loop);
    const now = performance.now();
    const dt = this.lastFrame ? now - this.lastFrame : 16;
    this.lastFrame = now;
    this.frameAccum += dt;
    this.frameCount++;
    if (this.frameAccum >= 500) {
      this.stats.fps = Math.round((this.frameCount * 1000) / this.frameAccum);
      this.frameAccum = 0;
      this.frameCount = 0;
    }

    // 布局过渡优先：它每帧都在改坐标，必须在绘制前跑完。
    this.layoutFrame?.();

    if (this.cameraAnimating) {
      const ease = Math.min(1, dt / 260);
      const cx = this.camera.x + (this.targetCamera.x - this.camera.x) * ease;
      const cy = this.camera.y + (this.targetCamera.y - this.camera.y) * ease;
      const ck = this.camera.k + (this.targetCamera.k - this.camera.k) * ease;
      const settled =
        Math.abs(cx - this.targetCamera.x) < 0.5 &&
        Math.abs(cy - this.targetCamera.y) < 0.5 &&
        Math.abs(ck - this.targetCamera.k) < 0.005;
      this.camera = settled ? { ...this.targetCamera } : { x: cx, y: cy, k: ck };
      this.hooks.onCamera?.(this.camera);
      if (settled) {
        this.cameraAnimating = false;
        if (this.focusPhase === "FOCUSING") this.focusPhase = "FOCUSED";
        if (this.focusPhase === "RESTORING") this.focusPhase = "IDLE";
      }
      this.dirty = true;
    }

    if (!this.dirty) return;
    this.dirty = false;
    const t0 = performance.now();
    this.draw();
    this.lastDrawMs = performance.now() - t0;
    this.checkFrameBudget();
  };

  /** REQ-103：单帧稳定超过 16.7ms 时提示降级（只提示一次，别反复烦人）。 */
  private checkFrameBudget() {
    this.frameMs = this.gl ? Math.max(this.gl.lastFrameMs, this.lastDrawMs) : this.lastDrawMs;
    if (this.frameMs <= 16.7) {
      this.slowFrames = 0;
      return;
    }
    this.slowFrames++;
    if (this.slowFrames > 45 && !this.slowHintFired) {
      this.slowHintFired = true;
      recordCrash({
        kind: "slow-frame",
        backend: this.backend,
        renderer: this.support.renderer || "unknown",
        detail: "连续掉帧：单帧耗时 " + String(Math.round(this.frameMs)) + "ms（超过 16.7ms）",
      });
      this.onSlowFrame?.(Math.round(this.frameMs));
    }
  }

  private draw() {
    if (this.gl) this.drawGL();
    else this.draw2D();
  }

  /** Canvas 2D 回退管线（REQ-103 的第二条渲染路径）。 */
  private draw2D() {
    const ctx = this.ctx;
    if (!ctx) return;
    const { width, height, dpr, camera } = this;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);

    // 底：纯浅蓝灰。网格用屏幕坐标画（不跟相机走），像一张钉住的方格纸；
    // 跟着平移会让整张纸"流动"，读图时反而晕。
    ctx.fillStyle = BLUEPRINT_CONFIG.bgColor;
    ctx.fillRect(0, 0, width, height);
    const grid = BLUEPRINT_CONFIG.gridSize;
    ctx.strokeStyle = BLUEPRINT_CONFIG.gridColor;
    ctx.lineWidth = 1;
    ctx.beginPath();
    // +0.5 让 1px 线落在像素中心，否则会糊成 2px 灰边。
    for (let x = 0.5; x < width; x += grid) {
      ctx.moveTo(x, 0);
      ctx.lineTo(x, height);
    }
    for (let y = 0.5; y < height; y += grid) {
      ctx.moveTo(0, y);
      ctx.lineTo(width, y);
    }
    ctx.stroke();

    const cx = width / 2;
    const cy = height / 2;
    const k = camera.k;

    const toScreen = (n: { x: number; y: number }) => ({
      x: (n.x - camera.x) * k + cx,
      y: (n.y - camera.y) * k + cy,
    });

    // 视口裁剪（REQ-034）：margin 50px
    const margin = 50;
    const visibleNodes = this.nodes.filter((n) => {
      if (n.hidden || !this.lodVisible(n)) return false;
      const s = toScreen(n);
      const rr = n.r * k + 24;
      return s.x > -margin - rr && s.x < width + margin + rr && s.y > -margin - rr && s.y < height + margin + rr;
    });
    this.stats.drawn = visibleNodes.length;
    this.stats.culled = this.nodes.filter((n) => !n.hidden).length - visibleNodes.length;

    const visSet = new Set(visibleNodes.map((n) => n.id));
    const selId = this.selected ? this.selected.id : null;
    const hovId = this.hovered ? this.hovered.id : null;
    const focusId = this.focus ? this.focus.id : null;

    // ---- 簇包络（语义"排序模式"）：每个簇一个淡色圆 + 簇名 --------------
    // 只在该布局下画：力导向时簇的边界本来就在浮动，画个死圆反而干扰。
    if (this.clusterMeta.length && this.layoutMode === "cluster") {
      this.drawClusterHulls(ctx, visibleNodes, toScreen, k);
    }

    // ---- 连线：一律直线。蓝图是工程图语言，曲线会让层级关系读不出来。
    // 指向文件的边用虚线（"叶子边"），目录之间的边保持实线，一眼能分主干与末端。
    ctx.lineCap = "butt";
    for (const e of this.edges) {
      const s = e.source as unknown as SimNode;
      const t = e.target as unknown as SimNode;
      if (!visSet.has(s.id) || !visSet.has(t.id)) continue;
      const a = toScreen(s);
      const b = toScreen(t);
      const related = selId !== null && (s.id === selId || t.id === selId);
      const focused = focusId !== null && (s.id === focusId || t.id === focusId);
      let dim = false;
      if (focusId !== null) {
        dim = !focused && !this.neighbors.has(s.id) && !this.neighbors.has(t.id);
      } else if (selId !== null) {
        dim = !related && !this.neighbors.has(s.id) && !this.neighbors.has(t.id);
      }
      // 后端只有 contains/similar 两种 edge_type，判不出"这一端是文件"，
      // 所以直接用端点类型判断，语义才和"层级=文件的边"对得上。
      const leafEdge = s.type === "file" || t.type === "file";
      ctx.setLineDash(leafEdge ? BLUEPRINT_CONFIG.dashPattern : []);
      ctx.beginPath();
      ctx.moveTo(a.x, a.y);
      ctx.lineTo(b.x, b.y);
      if (related || focused) {
        ctx.strokeStyle = BLUEPRINT_CONFIG.activeColor;
        ctx.lineWidth = BLUEPRINT_CONFIG.lineWidth + 1;
        ctx.globalAlpha = 1;
      } else {
        ctx.strokeStyle = BLUEPRINT_CONFIG.lineColor;
        ctx.lineWidth = BLUEPRINT_CONFIG.lineWidth;
        ctx.globalAlpha = dim ? 0.12 : 0.75;
      }
      ctx.stroke();
    }
    ctx.setLineDash([]);
    ctx.globalAlpha = 1;

    // ---- 节点：先小后大，文件夹盖在文件上面
    const sorted = visibleNodes.slice().sort((a, b) => a.r - b.r);
    for (const n of sorted) {
      let sx = (n.x - camera.x) * k + cx;
      let sy = (n.y - camera.y) * k + cy;
      let radius = Math.max(1.2, n.r * k);

      if (this.fisheye && this.pointer.x) {
        const dx = sx - this.pointer.x;
        const dy = sy - this.pointer.y;
        const d = Math.hypot(dx, dy);
        if (d < this.fisheyeRadius && d > 0.001) {
          const push = (1 - d / this.fisheyeRadius) * 42;
          sx += (dx / d) * push;
          sy += (dy / d) * push;
          radius *= 1 + (this.fisheyeScale - 1) * (1 - d / this.fisheyeRadius);
        }
      }

      const isSel = n.id === selId;
      const isHov = n.id === hovId;
      const isFocus = n.id === focusId;
      let dim = false;
      if (focusId !== null) {
        dim = !isFocus && !this.neighbors.has(n.id);
      } else if (selId !== null) {
        dim = !isSel && !this.neighbors.has(n.id);
      }

      // 无关节点整体压到 0.2：选中一颗子树时，其余内容只当背景，
      // 但仍要能看清轮廓，所以不直接隐藏。
      ctx.globalAlpha = dim ? 0.2 : 1;
      const active = isSel || isFocus || isHov;

      // ---- 语义模式：画「点」，不画卡片 ----------------------------------
      //
      // 为什么必须换：语义图有 250+ 个文件挤在 8 个簇里，每张卡片宽 120~210px。
      // 按"不重叠"排，整图要 4000px 宽，fit 之后缩放只有 20% —— 名字小到
      // 读不出来，卡片还互相压成一片糊。实测截图就是一堵卡片墙。
      //
      // 点云才是这类"看聚类结构"的图该有的形态：点小、能密集、簇的形状
      // 一眼可见。名字不常驻，**悬停时用一个浮层标出来**（见 gx-tip），
      // 需要看具体文件时点开右侧详情或双击打开。
      // 这也符合蓝图语言：圆点 + 描边本来就是工程图的记号，不是装饰。
      if (this.layoutMode === "cluster") {
        const rr = Math.max(3, Math.min(9, n.r * 0.32 * Math.min(1.6, k)));
        if (active) {
          // 选中/悬停的点给一圈光环，配合详情面板使用。
          ctx.beginPath();
          ctx.arc(sx, sy, rr + 6, 0, TAU);
          ctx.fillStyle = n.color;
          ctx.globalAlpha = 0.16;
          ctx.fill();
          ctx.globalAlpha = dim ? 0.2 : 1;
        }
        ctx.beginPath();
        ctx.arc(sx, sy, rr, 0, TAU);
        // 填充用簇色淡化版 + 深描边保证在浅蓝底上可读（对比度 ≥3:1）。
        ctx.fillStyle = n.color;
        ctx.globalAlpha = (dim ? 0.2 : 1) * (active ? 1 : 0.82);
        ctx.fill();
        ctx.globalAlpha = dim ? 0.2 : 1;
        ctx.strokeStyle = active ? BLUEPRINT_CONFIG.activeColor : BLUEPRINT_CONFIG.nodeBorder;
        ctx.lineWidth = active ? 2.5 : 1.4;
        ctx.stroke();
        ctx.globalAlpha = 1;
        continue;
      }

      const cap = capsuleFor(n.name || "", radius);

      if (!cap.showLabel) {
        // 缩得太小时胶囊会退化成一块糊，改成「白底彩色描边」的小圆点，
        // 至少还能分辨类型。
        ctx.beginPath();
        ctx.arc(sx, sy, radius, 0, TAU);
        ctx.fillStyle = BLUEPRINT_CONFIG.nodeFill;
        ctx.fill();
        ctx.strokeStyle = n.color;
        ctx.lineWidth = Math.max(1, Math.min(2.5, radius * 0.55));
        ctx.stroke();
        ctx.globalAlpha = 1;
        continue;
      }

      const bw = cap.halfW * 2;
      const bh = cap.halfH * 2;
      const bx = sx - cap.halfW;
      const by = sy - cap.halfH;
      const radiusCap = Math.min(cap.radius, Math.min(bw, bh) / 2);

      // 白卡片：先铺底，再把左侧类型色条裁进圆角内，最后压深色描边，
      // 顺序反了描边会被色条盖掉一个角。
      roundRectPath(ctx, bx, by, bw, bh, radiusCap);
      ctx.fillStyle = active ? BLUEPRINT_CONFIG.activeFill : BLUEPRINT_CONFIG.nodeFill;
      ctx.fill();

      const accent = Math.min(BLUEPRINT_CONFIG.accentWidth, bw * 0.28);
      ctx.save();
      ctx.clip();
      ctx.fillStyle = n.color;
      ctx.fillRect(bx, by, accent * 2, bh);
      ctx.restore();

      roundRectPath(ctx, bx, by, bw, bh, radiusCap);
      ctx.strokeStyle = active ? BLUEPRINT_CONFIG.activeColor : BLUEPRINT_CONFIG.nodeBorder;
      ctx.lineWidth = BLUEPRINT_CONFIG.nodeBorderWidth;
      ctx.stroke();

      // 文字落在色条右侧的剩余区域中央，色条才不会压住首字。
      ctx.font = "700 " + cap.fontPx.toFixed(1) + "px " + BLUEPRINT_FONT;
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillStyle = BLUEPRINT_CONFIG.textColor;
      ctx.fillText(cap.text, sx + accent / 2, sy + 0.5);
      ctx.globalAlpha = 1;
    }
  }

  /**
   * 画语义簇的包络圆与簇名。
   *
   * 为什么要画：簇团布局把 250 个文件分成 8 团，但没有界限的话，
   * 相邻两团的卡片会互相穿插，"分类分好"这件事就看不出来。
   * 一个淡淡的包络圆 + 簇名，把"这是同一类"直接画在图上。
   *
   * 用**凸包近似**（取成员到簇心的最大距离 + 卡片半宽）而不是真正的
   * 凸包算法：圆就够了，而且圆的绘制成本是常数。
   */
  private drawClusterHulls(
    ctx: CanvasRenderingContext2D,
    visible: SimNode[],
    toScreen: (n: { x: number; y: number }) => { x: number; y: number },
    k: number
  ) {
    const byCluster = new Map<number, SimNode[]>();
    for (const n of visible) {
      const cid = (n as any).clusterId as number | undefined;
      if (cid === undefined) continue;
      const arr = byCluster.get(cid);
      if (arr) arr.push(n);
      else byCluster.set(cid, [n]);
    }
    ctx.save();
    for (const meta of this.clusterMeta) {
      const members = byCluster.get(meta.id);
      if (!members || members.length < 2) continue;
      // 簇心：成员坐标的均值（屏幕坐标，便于直接算半径）。
      let cx = 0;
      let cy = 0;
      for (const n of members) {
        const p = toScreen(n);
        cx += p.x;
        cy += p.y;
      }
      cx /= members.length;
      cy /= members.length;
      let r = 0;
      for (const n of members) {
        const p = toScreen(n);
        r = Math.max(r, Math.hypot(p.x - cx, p.y - cy));
      }
      r += 56; // 给卡片半宽留余量（文件名最多到 105 半宽，取一半多一点）

      // 包络：极淡的填充 + 同色描边。颜色用簇色但透明度压到 0.1 以下 ——
      // 它只是"分区提示"，不该跟节点抢注意力。
      ctx.beginPath();
      ctx.arc(cx, cy, r, 0, TAU);
      ctx.fillStyle = meta.color;
      ctx.globalAlpha = 0.055;
      ctx.fill();
      ctx.globalAlpha = 0.34;
      ctx.strokeStyle = meta.color;
      ctx.lineWidth = 2;
      ctx.setLineDash([8, 8]);
      ctx.stroke();
      ctx.setLineDash([]);

      // 簇名写在上方（贴着圆弧），字号随缩放钳在 10~13px，保证任何缩放都能读。
      const fontPx = Math.max(10, Math.min(13, 11 * Math.pow(k, 0.15)));
      ctx.globalAlpha = 0.9;
      ctx.font = "700 " + fontPx.toFixed(1) + "px " + BLUEPRINT_FONT;
      ctx.textAlign = "center";
      ctx.textBaseline = "bottom";
      const label = meta.label + " · " + meta.size;
      const tw = ctx.measureText(label).width + 12;
      // 簇名画在包络圆的**正下方**，并抽稀成一个短标签。
      //
      // 【为什么改】原来画在圆上方居中，但网格排布下相邻簇上下只隔一个
      // 格子边缘，8 个簇的标签会横着连成一片、互相压住（实测截图里
      // 「… 26」和「… 12」两行挤在一起完全读不出来）。
      // 改到圆下方 + 只取首段（"文件A · 文件B · 文件C · 41" → "文件A 等 41 个"）
      // 后，每个标签只占一行短宽度，不会再跨到隔壁簇。
      const short = shortClusterLabel(meta.label, meta.size);
      ctx.font = "700 " + fontPx.toFixed(1) + "px " + BLUEPRINT_FONT;
      const stw = ctx.measureText(short).width + 14;
      const labelY = cy + r + fontPx * 1.55;
      ctx.fillStyle = "rgba(255,255,255,0.95)";
      ctx.fillRect(cx - stw / 2, labelY - fontPx * 1.25, stw, fontPx * 1.55);
      // 文字用簇色但压暗 28%：纯簇色在浅底上一个青绿/亮黄会掉到 2:1 以下。
      ctx.fillStyle = shade(meta.color, -0.28);
      ctx.fillText(short, cx, labelY);
    }
    ctx.restore();
  }

  /** WebGL 主管线（REQ-103）：点与线一次性提交 GPU，文字标签画在覆盖层。 */
  private drawGL() {
    const glr = this.gl;
    if (!glr) return;
    const { width, height, dpr, camera } = this;
    const cx = width / 2;
    const cy = height / 2;
    const k = camera.k;

    const toScreen = (n: { x: number; y: number }) => ({
      x: (n.x - camera.x) * k + cx,
      y: (n.y - camera.y) * k + cy,
    });

    const margin = 60;
    const visible: SimNode[] = [];
    for (const n of this.nodes) {
      if (n.hidden || !this.lodVisible(n)) continue;
      const s = toScreen(n);
      const rr = n.r * k + 26;
      if (s.x < -margin - rr || s.x > width + margin + rr) continue;
      if (s.y < -margin - rr || s.y > height + margin + rr) continue;
      visible.push(n);
    }
    this.stats.drawn = visible.length;
    this.stats.culled = this.nodes.filter((n) => !n.hidden).length - visible.length;

    const visSet = new Set(visible.map((n) => n.id));
    const edges: GEdge[] = [];
    for (const e of this.edges) {
      const s = e.source as unknown as SimNode;
      const t = e.target as unknown as SimNode;
      if (visSet.has(s.id) && visSet.has(t.id)) edges.push(e);
    }

    const fish = this.fisheye && this.pointer.x > 0;
    try {
      glr.draw({
        nodes: visible as unknown as GNode[],
        edges,
        camera,
        width,
        height,
        dpr,
        selectedId: this.selected ? this.selected.id : null,
        hoveredId: this.hovered ? this.hovered.id : null,
        focusId: this.focus ? this.focus.id : null,
        neighbors: this.neighbors,
        fisheye: fish
          ? { on: true, x: this.pointer.x, y: this.pointer.y, radius: this.fisheyeRadius, scale: this.fisheyeScale }
          : null,
      });
    } catch (err) {
      // 运行期丢失上下文（驱动重置、休眠唤醒）时回退 Canvas 2D，别让界面白屏。
      this.fallbackReason = "WebGL 绘制失败，已回退 Canvas 2D：" + String(err);
      recordCrash({
        kind: "draw-fail",
        backend: this.backend,
        renderer: this.support.renderer || "unknown",
        detail: this.fallbackReason,
      });
      this.gl?.destroy();
      this.gl = null;
      this.backend = "canvas2d";
      return;
    }
    this.drawLabels(visible, cx, cy, k, camera);
  }

  /** WebGL 模式下的文字标签层（主 canvas 已被 WebGL 独占）。 */
  private drawLabels(
    visible: SimNode[],
    cx: number,
    cy: number,
    k: number,
    camera: Camera
  ) {
    const ctx = this.labelCtx;
    if (!ctx) return;
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    ctx.clearRect(0, 0, this.width, this.height);
    const selId = this.selected ? this.selected.id : null;
    const focusId = this.focus ? this.focus.id : null;
    const hovId = this.hovered ? this.hovered.id : null;
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    for (const n of visible) {
      const isSel = n.id === selId;
      const isFocus = n.id === focusId;
      const isHov = n.id === hovId;
      const dim =
        focusId !== null ? !isFocus && !this.neighbors.has(n.id) : selId !== null ? !isSel && !this.neighbors.has(n.id) : false;
      const radius = Math.max(1.2, n.r * k);
      const cap = capsuleFor(n.name || "", radius);
      if (!cap.showLabel || dim) continue;
      const sx = (n.x - camera.x) * k + cx;
      const sy = (n.y - camera.y) * k + cy;
      const active = isSel || isFocus || isHov;
      // 白底浅框：文字在浅蓝网格上必须有个"纸片"托底，否则细笔画糊成一片。
      ctx.font = "700 " + cap.fontPx.toFixed(1) + "px " + BLUEPRINT_FONT;
      const tw = Math.min(cap.halfW * 2, ctx.measureText(cap.text).width + cap.fontPx);
      const ty = sy + radius + cap.fontPx * 0.9;
      // #ffeb3b 在白底上几乎不可读，所以高亮只体现在托底色上，字始终深色。
      ctx.fillStyle = active ? BLUEPRINT_CONFIG.activeFill : "rgba(255,255,255,0.8)";
      ctx.fillRect(sx - tw / 2, ty - cap.fontPx * 0.62, tw, cap.fontPx * 1.24);
      ctx.fillStyle = BLUEPRINT_CONFIG.textColor;
      ctx.fillText(cap.text, sx, ty);
    }
  }
}

/**
 * 把十六进制色按比例变暗（amount<0）或变亮（amount>0）。
 *
 * 为什么需要：簇色是高饱和的青/黄/品红，直接当标签色压在 #dcebf5 浅底上
 * 对比度常低于 3:1；压暗 28% 后全部达标。canvas 的 fillStyle 只吃颜色串，
 * 拿不到 CSS color-mix 的计算结果，所以必须自己算。
 */
function shade(hex: string, amount: number): string {
  const m = /^#?([0-9a-f]{6})$/i.exec((hex || "").trim());
  if (!m) return hex || "#2c3e50";
  const n = parseInt(m[1], 16);
  const t = amount < 0 ? 1 + amount : 1;
  const add = amount < 0 ? 0 : 255 * amount;
  const ch = (v: number) => Math.max(0, Math.min(255, Math.round(v * t + add)));
  return (
    "#" +
    [ch((n >> 16) & 255), ch((n >> 8) & 255), ch(n & 255)]
      .map((v) => v.toString(16).padStart(2, "0"))
      .join("")
  );
}

/**
 * 把"文件A · 文件B · 文件C · 41"压缩成一个短标签"文件A 等 41 个"。
 *
 * 簇名本来是用三个代表文件名拼的，长度动辄 40+ 字符，横跨半个包络圆。
 * 网格排布下相邻簇只隔一个格子，长标签必然互相压。
 * 只留第一个代表 + 簇规模，既认得出"这一团是什么"，宽度又只有原来的 1/3。
 */
function shortClusterLabel(label: string, size: number): string {
  const first = (label || "").split(" · ")[0].trim();
  if (!first) return `${size} 个`;
  return `${first} 等 ${size} 个`;
}
