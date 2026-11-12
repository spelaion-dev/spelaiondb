/**
 * Which running tabs the "close with an open transaction?" question lists as running (SDB-253).
 *
 * A tab with an open transaction is listed once, under its transaction: closing it rolls back
 * everything its session did since BEGIN, and a statement still running there is inside that
 * transaction, so it is part of the same loss. Listing it as running too said that loss twice,
 * and right after a statement had reported its transaction (before its run was marked finished)
 * it named a query that was not running anymore. Only running tabs without an open transaction
 * stay in the running list: closing them cancels a query whose work would otherwise be kept.
 */
export function runningOutsideTransactions(running: string[], openTransactions: string[]): string[] {
  const open = new Set(openTransactions);
  return running.filter((id) => !open.has(id));
}
