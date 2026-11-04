// 2D 图谱的「一键整理」布局计算（需求 #7 / #9）。
//
// 为什么要自己写而不是引 d3-hierarchy：
//   1. 项目里没有 d3-hierarchy，为一个布局引一个 30KB 的包不值得，
//      而它真正的价值（多种 tree 变体）这里只用得到两种；
//   2. 星图有**强制约束**：卡片是横条不是圆点，同层节点的间距必须按卡片
//      实际宽度算，否则文件名一长就叠在一起 —— d3.tree 的 nodeSize 是
//      定值，反而要打补丁；
//   3. 布局必须能"增量"：只有新展开的子树需要落位，旧节点保持不动
//      （用户盯着看的图不该整张重排）。
//
// 两种布局共用一棵"树骨架"：由 parent 关系重建父子链，再按层级分配坐标。
import { capsuleFor, type GNode } from "./model";

/** 树骨架：children 索引 + 根集合。图里可能有多个根（多个盘符）。 */
export interface Forest {
  roots: GNode[];
  children: Map<string, GNode[]>;
}

/**
 * 从节点的 parent 关系重建森林。
 *
 * 注意不能信 depth 字段：后端在"目录展开"分支里子目录恒为 0、文件恒为 1，
 * 直接按 depth 分层会把子目录排到父节点上面。这里只认 parent。
 */
export function buildForest(nodes: GNode[], visible: (n: GNode) => boolean = () => true): Forest {
  const children = new Map<string, GNode[]>();
  const roots: GNode[] = [];
  const byId = new Map(nodes.map((n) => [n.id, n]));

  // 两套数据源的父子关系不一样，必须都认：
  //  - 星图后端（star.rs）给的是显式 parent id；
  //  - 2D 图谱后端（graph_data）只给 path，父子关系只能从路径前缀推。
  // 只支持一种的话，切换数据源后整张图会变成 100 个并列的根。
  const parentOf = (n: GNode): string | null => {
    const explicit = (n as any).parentId as string | undefined;
    if (explicit && byId.has(explicit) && explicit !== n.id) return explicit;
    const p = normDir(n.path);
    if (!p) return null;
    // 目录节点：父 = 上一级目录的 id；文件节点：父 = 自己所在目录的 id。
    const want = n.type === "file" ? p : parentDir(p);
    if (!want) return null;
    if (byId.has(want)) return want;
    const prefixed = byId.has("f:" + want) ? "f:" + want : null;
    return prefixed;
  };

  for (const n of nodes) {
    if (!visible(n)) continue;
    const p = parentOf(n);
    if (!p) {
      roots.push(n);
      continue;
    }
    const arr = children.get(p);
    if (arr) arr.push(n);
    else children.set(p, [n]);
  }
  return { roots, children };
}

/** 去掉末尾反斜杠并统一大小写不敏感的比较口径（Windows 路径）。 */
function normDir(p: string): string {
  return (p || "").replace(/[\\/]+$/, "");
}

/** 取上一级目录；已经是盘符根（`D:`）时返回空串。 */
function parentDir(p: string): string {
  const i = Math.max(p.lastIndexOf("\\"), p.lastIndexOf("/"));
  if (i <= 0) return "";
  const head = p.slice(0, i);
  // `D:` → head 是 "D:"，它的父是空；`D:a` → head 是 "D:"，父是 `D:`
  if (/^[A-Za-z]:$/.test(head)) return head + "\\";
  return head;
}

/** 节点在世界坐标里的横向半宽：用胶囊卡片的估算宽度，卡片才不会叠。 */
function halfWidth(n: GNode): number {
  return capsuleFor(n.name || "", Math.max(4, n.r)).halfW;
}

/**
 * 层级树布局（蓝图风）：根在上，逐层向下铺开。
 *
 * 横向用「子树宽度累加」而不是均匀分列：一个节点占的宽度 = 自己卡片宽度
 * 与所有孩子宽度之和的较大者，这样长名字不会压到隔壁子树。
 * 纵向间距按层递增（越深的层文字越小，间距可以更紧）。
 */
export function layoutTree(
  nodes: GNode[],
  opts: { gapX?: number; gapY?: number; originX?: number; originY?: number } = {},
): Map<string, { x: number; y: number }> {
  const gapX = opts.gapX ?? 26;
  const gapY = opts.gapY ?? 96;
  const originX = opts.originX ?? 0;
  const originY = opts.originY ?? 0;
  const forest = buildForest(nodes);
  const out = new Map<string, { x: number; y: number }>();

  // 计算每棵子树的宽度（后序）。
  const widthMem = new Map<string, number>();
  const measure = (n: GNode): number => {
    const cached = widthMem.get(n.id);
    if (cached !== undefined) return cached;
    const self = halfWidth(n) * 2;
    const kids = forest.children.get(n.id) ?? [];
    if (!kids.length) {
      widthMem.set(n.id, self);
      return self;
    }
    let sum = 0;
    for (const k of kids) sum += measure(k) + gapX;
    sum -= gapX;
    const w = Math.max(self, sum);
    widthMem.set(n.id, w);
    return w;
  };

  // 放置（前序）：根在 y=0，孩子按子树宽度依次排开。
  let cursorRootX = originX;
  for (const root of forest.roots) {
    const totalW = measure(root);
    // 每个根整体居中到它自己的宽度盒里
    place(root, cursorRootX + totalW / 2, originY, 0);
    cursorRootX += totalW + gapX * 3;
  }

  function place(n: GNode, x: number, y: number, depth: number): void {
    out.set(n.id, { x, y });
    const kids = forest.children.get(n.id) ?? [];
    if (!kids.length) return;
    let sum = 0;
    for (const k of kids) sum += measure(k) + gapX;
    const span = sum - gapX;
    let cursor = x - span / 2;
    // 层的纵向间距随深度略收：深层节点更多，间距太大会把图拉得又高又空。
    const dy = gapY * Math.max(0.62, 1 - depth * 0.06);
    const childY = y + dy;
    for (const k of kids) {
      const kw = measure(k);
      place(k, cursor + kw / 2, childY, depth + 1);
      cursor += kw + gapX;
    }
  }

  return out;
}

/**
 * 径向树布局：根在圆心，每层一圈。
 *
 * 角度按「叶子数」分配而不是按兄弟数均分 —— 否则一棵有 200 个叶子的子树
 * 会和只有一个叶子的子树分到同样宽的角度，前者挤成一团、后者空一大片。
 */
export function layoutRadial(
  nodes: GNode[],
  opts: { radiusStep?: number; originX?: number; originY?: number } = {},
): Map<string, { x: number; y: number }> {
  const originX = opts.originX ?? 0;
  const originY = opts.originY ?? 0;
  const forest = buildForest(nodes);
  const out = new Map<string, { x: number; y: number }>();

  // 叶子数（后序）
  const leafMem = new Map<string, number>();
  const leaves = (n: GNode): number => {
    const c = leafMem.get(n.id);
    if (c !== undefined) return c;
    const kids = forest.children.get(n.id) ?? [];
    const v = kids.length ? kids.reduce((s, k) => s + leaves(k), 0) : 1;
    leafMem.set(n.id, v);
    return v;
  };

  // 半径步长：按最外层叶子数估个合适值，避免 500 个叶子全挤在一圈上。
  const totalLeaves = forest.roots.reduce((s, r) => s + leaves(r), 0);
  const maxDepth = (() => {
    let d = 0;
    const walk = (n: GNode, depth: number) => {
      d = Math.max(d, depth);
      for (const k of forest.children.get(n.id) ?? []) walk(k, depth + 1);
    };
    for (const r of forest.roots) walk(r, 0);
    return d;
  })();
  // 一圈的周长要放得下这一层的卡片：周长 = 2πr ≥叶子数 × 平均卡片宽。
  const avgW = nodes.length ? nodes.reduce((s, n) => s + halfWidth(n) * 2, 0) / nodes.length : 90;
  const autoStep = Math.max(140, Math.min(760, (totalLeaves * (avgW + 14)) / (2 * Math.PI * Math.max(1, maxDepth))));
  const step = opts.radiusStep ?? autoStep;

  // 多根：把 360° 平分给各根（按叶子数加权）。
  let angle = 0;
  const total = Math.max(1, totalLeaves);
  for (const root of forest.roots) {
    const share = (leaves(root) / total) * Math.PI * 2;
    place(root, angle, angle + share, 0);
    angle += share;
  }

  function place(n: GNode, a0: number, a1: number, depth: number): void {
    const mid = (a0 + a1) / 2;
    // 根在圆心：第一层直接居中，否则"根"和"第一层"会重合。
    const r = depth === 0 ? 0 : step * depth;
    out.set(n.id, {
      x: originX + Math.cos(mid) * r,
      y: originY + Math.sin(mid) * r,
    });
    const kids = forest.children.get(n.id) ?? [];
    if (!kids.length) return;
    let cursor = a0;
    const span = a1 - a0;
    for (const k of kids) {
      const share = (leaves(k) / Math.max(1, leaves(n))) * span;
      place(k, cursor, cursor + share, depth + 1);
      cursor += share;
    }
  }

  return out;
}

/**
 * 语义簇布局（"排序模式"专用）。
 *
 * 为什么不能用上面两个函数：语义图里的文件**没有父子关系**，每个节点
 * 都是根。tree/radial 会把 253 个"根"全部放在圆心（depth 0 → r = 0），
 * 结果就是 253 个卡片精确重叠成一坨 —— 实测截图里只看得见最上面那一个。
 *
 * 正确做法是**按簇分组**：每个簇先算一个簇心（围成一圈），簇内成员
 * 再用黄金角螺旋散在簇心周围。这样整张图是"若干星团 + 团内一点结构"，
 * 正好是用户要的"模型给它们分类分好、排序排好"。
 *
 * @param members 每个簇的节点列表（由调用方按 cluster id 分组）
 */
export function layoutClusters(
  members: Map<number, GNode[]>,
  opts: { originX?: number; originY?: number; aspect?: number } = {},
): Map<string, { x: number; y: number }> {
  const originX = opts.originX ?? 0;
  const originY = opts.originY ?? 0;
  const this_aspect = Math.max(0.6, Math.min(3, opts.aspect ?? 1.6));
  const out = new Map<string, { x: number; y: number }>();

  // 簇按大小降序，最大的簇先占位置（视觉上像主星团）。
  let groups = [...members.entries()]
    .filter(([, list]) => list.length > 0)
    .sort((a, b) => b[1].length - a[1].length);
  if (!groups.length) return out;

  // 小簇合并：少于"中位数一半"的簇并成一个"其他"团。
  //
  // 为什么必须在布局层再做一次（后端已经按空间分组了）：
  // 网格是 O(k) 的，k 从 9 涨到 15 就意味着 3×3 → 4×4，整图面积翻倍、
  // 缩放从 56% 掉到 35%。新冒出来的那些簇每个只有 5~12 个点，
  // 在画布上本来也只是一小撮 —— 并成一个"其他"团既省地方又更好读。
  //
  // 阈值用**中位数的一半**而不是"最大簇的百分比"：簇规模分布是长尾的
  // （实测 48/46/38/29/27/18/13/7/…），按最大簇算会把 13、18 也当成小簇
  // 合掉，主图只剩三个大团，反而丢信息；中位数能自动适配不同的分布。
  const sortedSizes = groups.map(([, list]) => list.length).sort((a, b) => a - b);
  const median = sortedSizes[Math.floor(sortedSizes.length / 2)] ?? 1;
  const minKeep = Math.max(6, Math.floor(median * 0.5));
  const keep = groups.filter(([, list]) => list.length >= minKeep);
  const small = groups.filter(([, list]) => list.length < minKeep);
  if (small.length > 1 && keep.length >= 2) {
    // 用 -1 作为合并后的簇 id（不会和后端的非负 id 冲突）。
    const merged = small.flatMap(([, list]) => list);
    groups = [...keep, [-1, merged] as [number, GNode[]]];
  }

  const golden = Math.PI * (3 - Math.sqrt(5));

  /**
   * 一组成员节点的平均"视觉半宽"。
   *
   * 语义节点的 r 只有 12~28，但卡片是横向的：真实占位由名字长度决定
   * （capsuleFor 算出来的 halfW 常到 60~105）。只按 r 算间距会让卡片
   * 互相压住，所以取两者较大值再平均。
   */
  const avgHalf = (list: GNode[]): number => {
    if (!list.length) return 40;
    return (
      list.reduce((s, n) => s + Math.max(n.r, halfWidth(n) * 0.55), 0) / list.length
    );
  };

  // 簇心位置：**按簇大小做网格排布**（而不是沿圆周铺一圈）。
  //
  // 【两次踩坑】
  //   1) 第一版按 1/sin(π/k) 算环半径，k 小时因子飙到 1，缩放压到 10%；
  //   2) 第二版改成"半径和 / 2π"，k 小时正常了，但**簇一多就线性膨胀** ——
  //      实测 15 个簇（8 个主簇 + 7 个跨空间小簇）时环半径 1000+，
  //      总半径 3600，缩放又被压到 19%，画布一片空白。
  //
  // 环半径本质上是 O(k) 的：k 个团沿一圈排，周长随 k 线性增长，半径也是。
  // 而网格排布只有 O(√k)，15 个簇只占 4×4 的网格，宽度约 4 个团直径 ——
  // 这才是"簇很多"时唯一能保持在视口内的方案。
  // 每个簇的外接半径：与团内排布用同一个面积法公式，两者必须一致，
  // 否则网格格子与团的实际大小对不上（格子过大 → 整图被撑散）。
  const groupR = groups.map(([, list]) => {
    const sp = 26;
    const n = Math.max(1, list.length);
    return sp * Math.sqrt((n * 0.866) / Math.PI) + 30;
  });

  // 格子边长取"平均团直径"而不是最大团直径：大簇会溢出一点邻格，
  // 但它周围通常是小簇，视觉上反而是"主星团被小团环绕"的自然结构。
  // 用最大团的话，一个小簇也要占满整个大格子，整图白白膨胀一倍。
  const cellW = (groupR.reduce((a, b) => a + b, 0) / groupR.length) * 2 + 60;
  // 列数按**视口宽高比**分配，而不是固定 √k。
  // 16:9 的屏幕用 4×4 会上下留出大片空白（图被高度限制压小）；
  // 按宽度多排几列、少排几行，能更充分地利用宽屏 —— 同样 12 个簇，
  // 4×3 比 4×4 少一整行，缩放能多出 25%。
  const aspect = this_aspect;
  const cols = Math.max(1, Math.ceil(Math.sqrt(groups.length * aspect)));
  const rows = Math.max(1, Math.ceil(groups.length / cols));

  // 让整体以原点为中心。
  const gridW = cols * cellW;
  const gridH = rows * cellW;

  groups.forEach(([, list], gi) => {
    // 网格坐标：大簇排前面（左上角先占），行列均为 √k 级。
    const col = gi % cols;
    const row = Math.floor(gi / cols);
    const cx = originX - gridW / 2 + cellW / 2 + col * cellW;
    const cy = originY - gridH / 2 + cellW / 2 + row * cellW;

    // 团内：黄金角（太阳花）螺旋。centrality 高的（r 大的）排内圈，
    // 让"代表性文件"更靠近簇心 —— 一眼能看出这一团的主干是什么。
    //
    // 【关键修正】半径必须按 `√(i/n)` 归一化，而不是 `√i`：
    //   √i 版本里最后一个点的半径 = spacing·√n，**与 n 的平方根成正比**，
    //   48 个点的团半径会到 366；而黄金角螺旋本来就能在"面积 ∝ n"的
    //   圆形区域里均匀铺点，正确半径是 spacing·√(n/π/0.866) ≈ spacing·√n·0.606，
    //   同一个团只要 211 —— 小了 42%，整张图立刻落回视口内。
    // centrality 高的排内圈：让"代表性文件"更靠近簇心。
    const sorted = list.slice().sort((a, b) => b.r - a.r);
    const n = sorted.length;
    // 语义图渲染成**点云**（不是卡片墙），单个点的直径只有 6~18px，
    // 所以间距按 26px 就够 —— 原来按卡片半宽算的 58+ 是为"排卡片"服务的，
    // 对点云来说等于把簇撑成一个大空圈。
    const spacing = 26;
    // 外接半径：面积法。每点占 spacing²×0.866（六边形密铺），πr² = n×该值。
    const rMax = spacing * Math.sqrt((n * 0.866) / Math.PI);
    sorted.forEach((node, i) => {
      if (n === 1) {
        out.set(node.id, { x: cx, y: cy });
        return;
      }
      const a = i * golden;
      // √(i/n) 把 i=0..n-1 均匀映射到 0..rMax 的圆盘上（面积均匀）。
      const rad = rMax * Math.sqrt(i / n);
      // 椭圆化：纵向压到 0.78。
      //
      // 0.62 压得太扁（实测相邻两排卡片的名字互相压住），0.78 是实测出来的
      // 折中：卡片高度约 34px、纵向间距 = spacingX×0.78 ≈ 65px，够放下两行；
      // 而横向仍保持 1.0，横长卡片之间也能让开。
      out.set(node.id, {
        x: cx + Math.cos(a) * rad,
        y: cy + Math.sin(a) * rad * 0.78,
      });
    });
  });

  return out;
}
