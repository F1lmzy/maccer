#ifndef MACCER_AGENT_H
#define MACCER_AGENT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct MaccerAgent MaccerAgent;
typedef struct MaccerFrontendClaim MaccerFrontendClaim;

typedef struct MaccerIconPixels {
    uint8_t *bgra;
    size_t len;
    uint32_t width;
    uint32_t height;
} MaccerIconPixels;

/* All handle operations except the string/icon free functions are serialized on the UI thread. */
MaccerAgent *maccer_agent_create(bool fixture);
/*
 * Opt in to an already-running Maccer service at an absolute Unix-socket path.
 * Returns a lightweight handle immediately; connection success/failure arrives
 * through poll as ready/error. This never starts a service or falls back to the
 * embedded backend. All normal send/poll/icon/destroy calls remain available.
 */
MaccerAgent *maccer_agent_connect(const char *socket_path);
/*
 * Connect to the service and claim the presentation lease for "gpui" or "pill".
 * Poll emits unsolicited {type:"present"} events independently of search IDs.
 * This never launches a frontend or service. The caller must still poll events.
 */
MaccerAgent *maccer_agent_connect_frontend(const char *socket_path, const char *frontend);
/*
 * Blocking native-app startup only, before creating windows. Enforces one
 * native frontend process per service and the backend-selected kind when
 * frontendControl is negotiated. Keep the guard for the app's lifetime.
 * Returns NULL on refusal/failure. error may be NULL; otherwise it receives
 * NULL or an owned string freed with maccer_agent_string_free(). Does not
 * start a backend, construct providers, or replace the async connect APIs.
 */
MaccerFrontendClaim *maccer_agent_claim_frontend(const char *socket_path, const char *frontend, char **error);
void maccer_agent_frontend_claim_free(MaccerFrontendClaim *claim);
void maccer_agent_destroy(MaccerAgent *agent);
/*
 * send accepts search {op, request, query}, activate {op, request,
 * search_request, provider, id, action, confirmed?}, details {op, request,
 * search_request, provider, id}, refresh {op, request, search_request}, cancel
 * {op}, and service-only home {op, request}. details request IDs use their own
 * monotonically increasing sequence; refresh request IDs use the main sequence.
 * search_request is
 * the request ID that produced the displayed result, distinct from the new
 * activation request ID. confirmed defaults to false; set it true only after
 * showing the result action's optional "confirmation" prompt. Accepted commands
 * return NULL; errors are allocated.
 */
char *maccer_agent_send(MaccerAgent *agent, const char *json);
/* Returns one allocated UTF-8 event, or NULL when the queue is empty. Besides
 * request-correlated events, poll may deliver unsolicited {type:"themeState",
 * state} events carrying the shared active palette and {type:"present"} when
 * this selected frontend receives a presentation request. They have no request
 * ID and are independent of search, details and cancellation. A separate
 * {type:"presentationError",message} reports control-channel failures. */
char *maccer_agent_poll(MaccerAgent *agent);
void maccer_agent_string_free(char *value);
/*
 * Copies pixels for a previously resolved application icon. This never invokes
 * AppKit or resolves an icon. Returns NULL for a missing/invalid token.
 * The caller owns the returned copy until maccer_agent_icon_free(). Pixels are
 * top-to-bottom BGRA, straight alpha, and at most 64x64. All handle operations
 * except the free functions are serialized on the UI thread.
 */
MaccerIconPixels *maccer_agent_icon(MaccerAgent *agent, const char *token);
void maccer_agent_icon_free(MaccerIconPixels *icon);

#ifdef __cplusplus
}
#endif
#endif
