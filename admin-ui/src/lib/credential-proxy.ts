import type { AddCredentialRequest, CredentialStatusItem } from '../types/api'

export type ProxyChoice =
  | { kind: 'global' }
  | { kind: 'direct' }
  | { kind: 'pool'; id: number }
  | { kind: 'custom'; url: string }

export type ImportProxyChoice = ProxyChoice | { kind: 'preserve' }
export type CredentialProxyFields = Pick<AddCredentialRequest,
  'proxyId' | 'proxyUrl' | 'proxyUsername' | 'proxyPassword'>

export function proxyChoiceOf(credential: CredentialStatusItem): ProxyChoice {
  if (credential.proxyId != null) return { kind: 'pool', id: credential.proxyId }
  const url = credential.proxyUrl?.trim() ?? ''
  if (!url) return { kind: 'global' }
  if (url.toLowerCase() === 'direct') return { kind: 'direct' }
  return { kind: 'custom', url }
}

export function sameProxyChoice(a: ProxyChoice, b: ProxyChoice): boolean {
  if (a.kind !== b.kind) return false
  if (a.kind === 'pool' && b.kind === 'pool') return a.id === b.id
  if (a.kind === 'custom' && b.kind === 'custom') return a.url === b.url
  return true
}

export function proxyRequestFields(choice: ProxyChoice): { proxyId?: number; proxyUrl?: string } {
  switch (choice.kind) {
    case 'pool': return { proxyId: choice.id }
    case 'direct': return { proxyUrl: 'direct' }
    case 'global': return { proxyUrl: '' }
    case 'custom': return {}
  }
}

/** A chosen route overrides every file field, so stale authentication cannot follow it. */
export function importProxyFields(
  choice: ImportProxyChoice,
  source: CredentialProxyFields,
): CredentialProxyFields {
  if (choice.kind !== 'preserve') return proxyRequestFields(choice)
  return {
    proxyId: source.proxyId,
    proxyUrl: source.proxyUrl?.trim() || undefined,
    proxyUsername: source.proxyUsername?.trim() || undefined,
    proxyPassword: source.proxyPassword?.trim() || undefined,
  }
}
