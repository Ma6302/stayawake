// stayawake 的 DSH (DeepSeek Harness) 适配插件
//
// 为什么需要它: 和 OpenCode 一样, DSH 是 Electron 应用。模型思考期间没有工具在跑、
// 磁盘与网络 I/O 接近零, CPU 几乎全被 UI 重绘吃掉 —— 被动检测抓不到这种状态。
// 只有宿主自己知道"还有 agent 没跑完", 所以由宿主在忙碌期间把 hint 文件撑住。
//
// 覆盖范围: 每个 agent 从 status 变成 running 到回到 idle 全程保持唤醒 ——
// 模型思考(没有工具、CPU 也不高)、流式输出、工具执行, 以及被派生的子 agent
// (子 agent 也是 agent, 同样发 agent/status)。多 agent 并发时引用计数,
// 全部 idle 才释放。
//
// 机制: 忙碌时保持 hint 文件新鲜, 全部结束后删除。stayawake 看 mtime 判定,
// mtime 超过 TTL(默认 60s) 自动失效 —— 本插件崩溃不会把机器永久卡醒。
import { mkdir, rm, writeFile } from "node:fs/promises"
import { join } from "node:path"

export const name = "stayawake-dsh-hint"

const HINT_DIR = join(process.env.LOCALAPPDATA ?? "", "stayawake", "hints")
// 一个 profile 一个文件名: 同时开两个 profile 的 DSH 时, 各自的启动清理
// 不会把对方正撑着的 hint 删掉。DSH_PROFILE 只在 profile 启动时有值。
const PROFILE = (process.env.DSH_PROFILE ?? "").replace(/[^A-Za-z0-9._-]/g, "_")
const HINT_FILE = join(HINT_DIR, PROFILE ? `dsh-${PROFILE}.hint` : "dsh.hint")
// 刷新间隔要明显小于 stayawake 的 hint_ttl_secs(默认 60s)
const REFRESH_MS = 20_000

export function apply(ctx) {
  /** 未 idle 的 agent -> 它在忙什么。用 agent 对象本身当键, 不依赖任何 id 字段 */
  const busy = new Map()
  let timer = null
  // sync 是否正在执行, 以及执行期间是否又有新请求(合并用)
  let running = false
  let again = false

  // 把 hint 文件同步到**当前** busy 状态: 有 agent 在跑就刷新文件(更新 mtime),
  // 全空了就删除。
  //
  // 关键: 写还是删, 必须按**执行那一刻**的 busy 决定, 且全程串行。否则会出现
  // 写-删竞态 —— mark 的异步写(mkdir+writeFile 两步)与 done 的异步删(一步 rm)
  // 并发时, 删先完成、写后落地, 于是文件在所有 agent 停稳后残留, 定时器已停,
  // 只能等 TTL(60s) 过期。OpenCode 那边就是这么暴露的: 空闲后托盘仍显示
  // hint:opencode.hint(thinking), 过一会才消失。
  //
  // running/again 做合并: 状态跃变很密集时, 在途的调用只置 again, 结束后补一轮,
  // 免得把上百次写排进队列。补的那一轮读的是最新 busy, 所以状态一定收敛。
  const sync = async () => {
    if (running) {
      again = true
      return
    }
    running = true
    try {
      do {
        again = false
        try {
          if (busy.size > 0) {
            const reason = [...busy.values()].join(", ")
            await mkdir(HINT_DIR, { recursive: true })
            await writeFile(HINT_FILE, `${reason}\n${new Date().toISOString()}\n`)
          } else {
            await rm(HINT_FILE, { force: true })
          }
        } catch {
          // stayawake 没装/没跑都不影响 DSH 本身
        }
      } while (again)
    } finally {
      running = false
    }
  }

  /** 标记某 agent 在忙。 */
  const mark = (agent) => {
    busy.set(agent ?? "-", "running")
    if (!timer) {
      // 模型思考期间可能长时间没有任何事件, 定时器保证 mtime 不过期。
      // 定时器只管触发 sync, 由 sync 按 busy 判断写还是删。
      timer = setInterval(() => {
        sync()
      }, REFRESH_MS)
      timer.unref?.()
    }
    return sync()
  }

  /** 某 agent 结束。全部结束后才释放。 */
  const done = (agent) => {
    busy.delete(agent ?? "-")
    if (busy.size === 0 && timer) {
      clearInterval(timer)
      timer = null
    }
    return sync()
  }

  const disposers = [
    // AgentStatus 只有 'idle' | 'running' 两种取值, 且"无操作的状态跃变"不会发事件
    // (dsh-agent 的 invariant 会因此报错), 所以这一条就覆盖了模型思考、流式输出、
    // 工具执行与子 agent 的全部忙碌区间。
    ctx.on("agent/status", ({ agent, status }) => {
      void (status === "running" ? mark(agent) : done(agent))
    }),
    // 插件在某个 agent 已经在跑之后才被装上时的兜底: 补一次当前状态。
    ctx.on("agent/created", ({ agent }) => {
      if (agent?.status === "running") void mark(agent)
    }),
    // agent 销毁后不会再有 status 事件, 必须自己释放, 否则会一直卡着。
    ctx.on("agent/disposed", ({ agent }) => {
      void done(agent)
    }),
  ]

  // 启动时清掉上次异常退出留下的陈旧文件(busy 为空 -> sync 走删除分支)
  void sync()

  return () => {
    for (const dispose of disposers) {
      try {
        dispose()
      } catch {
        // 卸载路径上的异常不该往外抛
      }
    }
    if (timer) {
      clearInterval(timer)
      timer = null
    }
    busy.clear()
    // 卸载后 hint 不能留着, 否则还要等 TTL 才失效
    return sync()
  }
}
