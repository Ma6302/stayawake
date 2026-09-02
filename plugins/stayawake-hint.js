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

  const write = async () => {
    const reason = busy.size ? [...busy.values()].join(", ") : "busy"
    try {
      await mkdir(HINT_DIR, { recursive: true })
      await writeFile(HINT_FILE, `${reason}\n${new Date().toISOString()}\n`)
    } catch {
      // stayawake 没装/没跑都不影响 OpenCode 本身
    }
  }

  const clear = async () => {
    try {
      await rm(HINT_FILE, { force: true })
    } catch {}
  }

  /** 标记某会话在忙。sessionID 缺失时用固定键, 至少不会漏。 */
  const mark = async (sessionID, what) => {
    busy.set(sessionID ?? "-", what)
    if (!timer) {
      // 模型思考期间可能长时间没有任何事件, 定时器保证 mtime 不过期
      timer = setInterval(() => {
        if (busy.size) write()
      }, REFRESH_MS)
      timer.unref?.()
    }
    await write()
  }

  /** 某会话结束。全部结束后才释放。 */
  const done = async (sessionID) => {
    busy.delete(sessionID ?? "-")
    if (busy.size === 0) {
      if (timer) {
        clearInterval(timer)
        timer = null
      }
      await clear()
    } else {
      await write()
    }
  }

  // 启动时清掉上次异常退出留下的陈旧文件
  await clear()

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
