#include <SoundPlayer.h>
#include <MediaDefs.h>
#include <new>
#include <stdint.h>

typedef void (*HaikuFillFn)(void* cookie, void* buf, size_t size);

struct HaikuCtx { HaikuFillFn fill_fn; void* user_cookie; };

static void bsp_callback(void* cookie, void* buf, size_t size,
                          const media_raw_audio_format& /*fmt*/)
{
    HaikuCtx* ctx = static_cast<HaikuCtx*>(cookie);
    ctx->fill_fn(ctx->user_cookie, buf, size);
}

extern "C" {

// desired_rate IN; actual negotiated rate written to *out_rate.
void* haiku_player_create(HaikuFillFn fill_fn, void* user_cookie,
                          uint32_t desired_rate, uint32_t* out_rate)
{
    media_raw_audio_format fmt;
    fmt.frame_rate    = (float)desired_rate;
    fmt.channel_count = 2;
    fmt.format        = media_raw_audio_format::B_AUDIO_FLOAT;
    fmt.byte_order    = B_MEDIA_HOST_ENDIAN;
    fmt.buffer_size   = 0;

    HaikuCtx* ctx = new (std::nothrow) HaikuCtx{fill_fn, user_cookie};
    if (!ctx) return nullptr;
    BSoundPlayer* p = new (std::nothrow) BSoundPlayer(&fmt, "pianeer",
                                                       bsp_callback, nullptr, ctx);
    if (!p || p->InitCheck() != B_OK) { delete ctx; delete p; return nullptr; }
    if (out_rate) *out_rate = (uint32_t)fmt.frame_rate;
    return p; // ctx is process-lifetime; intentionally leaked on destroy
}

void haiku_player_start(void* h)   { auto p = static_cast<BSoundPlayer*>(h); p->Start(); p->SetHasData(true); }
void haiku_player_stop(void* h)    { static_cast<BSoundPlayer*>(h)->Stop(); }
void haiku_player_destroy(void* h) { delete static_cast<BSoundPlayer*>(h); }

} // extern "C"
