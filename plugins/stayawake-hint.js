// stayawake 的 OpenCode 适配插件
//
// 为什么需要它: OpenCode 是 Electron 应用, 实测「工具执行中」时 CPU 52-70% 几乎
// 全被 UI 重绘占掉, 真正干活的 node 进程只有 0.3%; 而 renderer↔gpu 的共享内存
// 流量能到 7 MB/s 却一字节未落盘。所以 CPU 与 I/O 都无法判断它是否真在忙。
//
// 覆盖范围: 会话从"开始处理"到 session.idle 之间全程保持唤醒 ——
// 包括模型思考(此时没有任何工具在跑、CPU 也不高)、流式输出、工具执行。
// 只要还有会话没 idle 就不让机器睡。
//
// 机制: 忙碌时保持 hint 文件新鲜, 全部会话结束后删除。stayawake 看 mtime 判定。
// mtime 超过 TTL(默认 60s) 自动失效 —— 本插件崩溃不会把机器永久卡醒。
import { mkdir, rm, writeFile } from "node:fs/promises"
import { join } from "node:path"

const HINT_DIR = join(process.env.LOCALAPPDATA ?? "", "stayawake", "hints")
const HINT_FILE = join(HINT_DIR, "opencode.hint")
// 刷新间隔要明显小于 stayawake 的 hint_ttl_secs(默认 60s)
const REFRESH_MS = 20_000

export const StayAwakeHint = async () => {
  /** 未 idle 的会话 ID -> 它在忙什么 */
  const busy = new Map()
  let timer = null
  // sync 是否正在执行, 以及执行期间是否又有新请求(合并用)
  let running = false
  let again = false

  // 把 hint 文件同步到**当前** busy 状态: 有会话在忙就刷新文件(更新 mtime),
  // 全空了就删除。
  //
  // 关键: 写还是删, 必须按**执行那一刻**的 busy 决定, 且全程串行。否则会出现
  // 写-删竞态 —— mark 的异步写(mkdir+writeFile 两步)与 done 的异步删(一步 rm)
  // 并发时, 删先完成、写后落地, 于是文件在会话结束后残留, 定时器已停, 只能等
  // TTL(60s) 过期。实测现象: opencode 空闲后托盘仍显示
  // hint:opencode.hint(thinking), 过一会才消失。
  //
  // running/again 做合并: 流式输出高频触发, 在途时后来的调用只置 again, 结束后
  // 补一轮即可, 免得把上百次写排进队列。补的那一轮读的是最新 busy, 所以状态一定收敛。
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
          // stayawake 没装/没跑都不影响 OpenCode 本身
        }
      } while (again)
    } finally {
      running = false
    }
  }

  /** 标记某会话在忙。sessionID 缺失时用固定键, 至少不会漏。 */
  const mark = (sessionID, what) => {
    busy.set(sessionID ?? "-", what)
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

  /** 某会话结束。全部结束后才释放。 */
  const done = (sessionID) => {
    busy.delete(sessionID ?? "-")
    if (busy.size === 0 && timer) {
      clearInterval(timer)
      timer = null
    }
    return sync()
  }

  // 启动时清掉上次异常退出留下的陈旧文件(busy 为空 -> sync 走删除分支)
  await sync()

  return {
    "tool.execute.before": async (input) => mark(input.sessionID, `tool:${input.tool}`),
    "tool.execute.after": async (input) => mark(input.sessionID, `tool:${input.tool}`),

    event: async ({ event }) => {
      const p = event.properties ?? {}
      switch (event.type) {
        // session.status 是覆盖"思考中"的关键: busy 表示会话正在被处理,
        // 此时可能既没有工具在跑也没有输出, 纯 CPU/网络检测抓不到。
        case "session.status":
          if (p.status?.type === "busy" || p.status?.type === "retry") {
            await mark(p.sessionID, "thinking")
          } else if (p.status?.type === "idle") {
            await done(p.sessionID)
          }
          break

        // 模型流式输出中
        case "message.part.updated":
          await mark(p.part?.sessionID ?? p.sessionID, "streaming")
          break

        // 会话真正结束
        case "session.idle":
          await done(p.sessionID)
          break

        // 出错/被删也要释放, 否则会一直卡着
        case "session.error":
          await done(p.sessionID)
          break
        case "session.deleted":
          await done(p.info?.id)
          break
      }
    },
  }
}
