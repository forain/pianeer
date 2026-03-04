#include <MidiConsumer.h>
#include <MidiRoster.h>
#include <MidiProducer.h>
#include <new>
#include <stdint.h>

typedef void (*MidiNoteFn)(void* cookie, uint8_t ch, uint8_t note, uint8_t vel);
typedef void (*MidiCCFn)  (void* cookie, uint8_t ch, uint8_t ctrl, uint8_t val);

class PianeerConsumer : public BMidiLocalConsumer {
public:
    void* cookie; MidiNoteFn note_on_fn, note_off_fn; MidiCCFn cc_fn;
    PianeerConsumer() : BMidiLocalConsumer("pianeer"),
        cookie(nullptr), note_on_fn(nullptr), note_off_fn(nullptr), cc_fn(nullptr) {}
    void NoteOn(uchar ch, uchar note, uchar vel, bigtime_t) override
        { if (note_on_fn) note_on_fn(cookie, ch, note, vel); }
    void NoteOff(uchar ch, uchar note, uchar vel, bigtime_t) override
        { if (note_off_fn) note_off_fn(cookie, ch, note, vel); }
    void ControlChange(uchar ch, uchar ctrl, uchar val, bigtime_t) override
        { if (cc_fn) cc_fn(cookie, ch, ctrl, val); }
};

extern "C" {

void* haiku_midi_consumer_create(void* cookie,
                                  MidiNoteFn note_on, MidiNoteFn note_off, MidiCCFn cc)
{
    PianeerConsumer* c = new (std::nothrow) PianeerConsumer();
    if (!c) return nullptr;
    c->cookie = cookie; c->note_on_fn = note_on; c->note_off_fn = note_off; c->cc_fn = cc;
    c->Register();

    int32 id = 0;
    BMidiProducer* prod;
    while ((prod = BMidiRoster::NextProducer(&id)) != nullptr) {
        if (!prod->IsLocal()) {
            prod->Connect(c);
            prod->Release();
            break;
        }
        prod->Release();
    }
    return c;
}

void haiku_midi_consumer_destroy(void* h) {
    auto* c = static_cast<PianeerConsumer*>(h);
    c->Unregister(); c->Release();
}

} // extern "C"
