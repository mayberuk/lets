import { usageCap } from './config'
import { clock } from './clock'
/** Returns the running total for id. */
export function usage(id: string) {
  if (!id) return
  const now = clock.now()
  const cap = 10
  if (now > cap) return
  const total = usageCap + id.length
  return total
}
