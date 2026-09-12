import { useCallback, useEffect, useRef, useState } from 'react'

export interface Message {
  id: number
  text: string
}

export interface Page {
  messages: Message[]
  hasMore: boolean
}

export interface FetchPageParams {
  before?: number
  limit: number
}

export interface MessageFeedProps {
  fetchPage: (params: FetchPageParams) => Promise<Page>
  initialLimit?: number
}

export default function MessageFeed({ fetchPage, initialLimit = 50 }: MessageFeedProps) {
  const [messages, setMessages] = useState<Message[]>([])
  const [hasMore, setHasMore] = useState(true)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const containerRef = useRef<HTMLDivElement>(null)
  const prevScrollHeightRef = useRef<number | null>(null)

  const loadInitial = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const page = await fetchPage({ limit: initialLimit })
      setMessages(page.messages)
      setHasMore(page.hasMore)
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }, [fetchPage, initialLimit])

  const loadEarlier = useCallback(async () => {
    if (loading || !hasMore || messages.length === 0) return
    setLoading(true)
    setError(null)
    const container = containerRef.current
    prevScrollHeightRef.current = container?.scrollHeight ?? null
    try {
      const oldest = messages[0].id
      const page = await fetchPage({ before: oldest, limit: initialLimit })
      setMessages((prev) => [...page.messages, ...prev])
      setHasMore(page.hasMore)
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }, [loading, hasMore, messages, fetchPage, initialLimit])

  useEffect(() => {
    loadInitial()
  }, [loadInitial])

  // 加载完成后恢复滚动位置���顶部前置消息后不跳动）
  useEffect(() => {
    const container = containerRef.current
    const prev = prevScrollHeightRef.current
    if (container && prev !== null) {
      container.scrollTop = container.scrollHeight - prev
    }
  }, [messages])

  const onScroll = () => {
    const container = containerRef.current
    if (!container) return
    if (container.scrollTop < 60) loadEarlier()
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      <div
        ref={containerRef}
        onScroll={onScroll}
        style={{ overflowY: 'auto', flex: 1, border: '1px solid #ccc', padding: '8px' }}
      >
        {hasMore ? (
          <div onClick={loadEarlier} style={{ textAlign: 'center', color: '#888', cursor: 'pointer', padding: 8 }}>
            {loading ? '加载中…' : '向上滚动加载更早消息'}
          </div>
        ) : (
          <div style={{ textAlign: 'center', color: '#aaa', padding: 8 }}>— 没有更早的消息了 —</div>
        )}
        {error && <div style={{ color: 'red', textAlign: 'center' }}>加载失败: {error}</div>}
        <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
          {messages.map((m) => (
            <li key={m.id} style={{ padding: '6px 4px', borderBottom: '1px solid #eee' }}>
              <span style={{ color: '#888', marginRight: 8 }}>#{m.id}</span>
              {m.text}
            </li>
          ))}
        </ul>
      </div>
      <div style={{ padding: 6, color: '#666', fontSize: 12 }}>
        已加载 {messages.length} 条 · {hasMore ? '还有更早消息' : '已到最早'}
      </div>
    </div>
  )
}
