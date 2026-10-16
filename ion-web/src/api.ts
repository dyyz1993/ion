const BASE = "http://127.0.0.1:8792"

export interface InterruptedSession {
  id: string
  name?: string
  project?: string
  model?: string
  agent?: string
  turns?: number
  updated_at?: string
}

// GET /api/interrupted → 2h 内有活动但无活跃 worker 的会话（断点）
export async function fetchInterrupted(): Promise<InterruptedSession[]> {
  const r = await fetch(BASE + "/api/interrupted")
  if (!r.ok) throw new Error(`interrupted: HTTP ${r.status}`)
  const d = await r.json()
  return d.interrupted ?? []
}

// 明确不是持久恢复：创建新的 build worker 做接力任务
export async function startRelayTask(sessionId: string, context: string): Promise<{ sessionId?: string; workerId?: string }> {
  const r = await fetch(BASE + "/api/create_worker", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      agent: "build",
      initial_prompt: `你在会话 ${sessionId} 断点续作：${context}`,
    }),
  })
  const d = await r.json().catch(() => ({}))
  if (!r.ok) throw new Error(d.error || `resumeSession: HTTP ${r.status}`)
  return d
}
