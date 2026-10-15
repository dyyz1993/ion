import { useEffect, useState } from "react"
import { fetchInterrupted, startRelayTask, type InterruptedSession } from "./api"

const RESUME_CTX = "检查 git status 和最近改动，继续未完成的工作"

// 中断会话面板：只提供诚实的“新建接力任务”，不冒充持久恢复
export default function InterruptedPanel() {
  const [items, setItems] = useState<InterruptedSession[]>([])
  const [err, setErr] = useState<string | null>(null)
  const [resumed, setResumed] = useState<Record<string, boolean>>({})
  const [busy, setBusy] = useState<string | null>(null)
  const [toast, setToast] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    const load = () =>
      fetchInterrupted()
        .then((d) => { if (alive) { setItems(d); setErr(null) } })
        .catch((e) => { if (alive) setErr(String(e.message || e)) })
    load()
    const t = window.setInterval(load, 15000)
    return () => { alive = false; window.clearInterval(t) }
  }, [])

  useEffect(() => {
    if (!toast) return
    const t = window.setTimeout(() => setToast(null), 5000)
    return () => window.clearTimeout(t)
  }, [toast])

  const doResume = (s: InterruptedSession) => {
    setBusy(s.id); setErr(null)
    startRelayTask(s.id, RESUME_CTX)
      .then(() => { setResumed((r) => ({ ...r, [s.id]: true })); setToast(`已新建接力任务：${s.name || s.id}`) })
      .catch((e) => setErr(String(e.message || e)))
      .finally(() => setBusy(null))
  }

  const fmtTime = (iso?: string) => {
    if (!iso) return "—"
    const d = new Date(iso)
    if (isNaN(d.getTime())) return iso
    const m = Math.round((Date.now() - d.getTime()) / 60000)
    return m < 60 ? `${m} 分钟前` : `${Math.round(m / 60)} 小时前`
  }

  return (
    <div style={{ border: "1px solid #333", borderRadius: 8, padding: "10px 14px", marginBottom: 12 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 10, marginBottom: 8 }}>
        <b style={{ fontSize: 13 }}>⚡ 断点复原</b>
        <span style={{ fontSize: 12, color: "#888" }}>发现中断的子任务，可新建接力任务</span>
        {toast && <span style={{ marginLeft: "auto", fontSize: 12, color: "#34d399" }}>{toast}</span>}
      </div>
      {err && <div style={{ fontSize: 12, color: "#f87171", marginBottom: 6 }}>{err}</div>}
      {items.length === 0 && !err ? (
        <div style={{ fontSize: 12, color: "#888", padding: "6px 0" }}>无断点会话</div>
      ) : (
        items.map((s) => (
          <div key={s.id} style={{ display: "flex", alignItems: "center", gap: 10, padding: "6px 0", borderTop: "1px solid #eee" }}>
            <span style={{ fontSize: 12, fontWeight: 600 }}>{s.name || s.id.slice(0, 12)}</span>
            <span style={{ fontSize: 11, color: "#888" }}>{s.project || "—"}</span>
            <span style={{ fontSize: 11, color: "#888" }}>turns {s.turns ?? "—"}</span>
            <span style={{ fontSize: 11, color: "#888" }}>最后活跃 {fmtTime(s.updated_at)}</span>
            <button
              disabled={!!resumed[s.id] || busy === s.id}
              onClick={() => doResume(s)}
              style={{ marginLeft: "auto", fontSize: 12, padding: "3px 10px", cursor: resumed[s.id] ? "default" : "pointer", borderRadius: 6 }}
            >{resumed[s.id] ? "已新建" : busy === s.id ? "创建中…" : "新建接力任务"}</button>
          </div>
        ))
      )}
    </div>
  )
}
