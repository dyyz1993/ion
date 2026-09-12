import { useCallback } from 'react'
import MessageFeed, { FetchPageParams, Message, Page } from './MessageFeed'

// mock: 生成 500 条假消息, id 1..500, 500 最新
const MOCK_MESSAGES: Message[] = Array.from({ length: 500 }, (_, i) => ({
  id: i + 1,
  text: `消息 #${i + 1}:这是第 ${i + 1} 条模拟消息`,
}))

// 模拟异步分页接口:不传 before 返回最新 limit 条;传 before 返回 id < before 的最大 limit 条
async function mockFetchPage({ before, limit }: FetchPageParams): Promise<Page> {
  await new Promise((r) => setTimeout(r, 300))
  let pool = MOCK_MESSAGES
  if (before !== undefined) {
    pool = MOCK_MESSAGES.filter((m) => m.id < before)
  }
  const messages = pool.slice(-limit)
  const hasMore = messages.length > 0 && messages[0].id > MOCK_MESSAGES[0].id
  return { messages, hasMore }
}

export default function App() {
  const fetchPage = useCallback(mockFetchPage, [])
  return (
    <div style={{ maxWidth: 640, margin: '0 auto', height: '100vh', display: 'flex', flexDirection: 'column' }}>
      <h3 style={{ margin: '8px 0' }}>ION Web — 消息流</h3>
      <div style={{ flex: 1, minHeight: 0 }}>
        <MessageFeed fetchPage={fetchPage} />
      </div>
    </div>
  )
}
