import { rates } from './rates'

export function computeFee(amount: number): number {
  const rate = rates.base
  return amount * rate
}

export class Cart {
  total(items: number[]): number {
    let sum = 0
    for (const item of items) {
      sum += computeFee(item)
    }
    return sum
  }
}

export function other() {
  return 1
}
