import { usageLimit } from './config'
import { clock } from './clock'
export function usage(id: string) {
  if (!id) return
  const now = clock.now()
  const cap = 10
  if (now > cap) return
  const total = usageLimit + id.length
  return total
}
