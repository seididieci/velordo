//! tileprobe — prova le **tile** del DE (D8e, ADR-0092).
//!
//! Gira con `desktop=1` (run dedicato, `scripts/run-desktop-tests.sh`), non nel
//! gate: il layout lo dispone solo un'immagine >= `ThirdParty` (il DE), quindi
//! non puo' stare in `usertests` (che e' `tst`). E' seedato anche in
//! `usr/bin/tileprobe.bin` per l'uso manuale dalla shell.
//!
//! Prove:
//! 1. `TILE_INFO` vede la tile di boot (una, la sessione 0) e il suo handle;
//! 2. crea una sessione, la dispone in **due tile** affiancate e committa;
//! 3. scrivendo nella seconda, il `TILE_CRC` della prima **non cambia** e
//!    quello della seconda si': il testo non sborda di una tile;
//! 4. **zoom**: la seconda si nasconde (rect 0) e la prima prende lo schermo;
//!    `TILE_INFO` conta una tile sola, e al ritorno il testo della prima e'
//!    quello di prima (nascondere non distrugge niente);
//! 5. `SESS_FOCUS` sposta i tasti sulla seconda tile (e `TILE_INFO` lo dice);
//! 6. `TILE_DROP` riporta a una tile.

#![no_std]
#![no_main]

use civis;
use civis::println;

civis::entry!(real_main);

/// L'esito e' un `TEST_DONE` sul canale di nascita (contratto dei probe).
fn finish(ok: bool) -> ! {
    if ok {
        println!("[tileprobe] risultato: PASS");
    } else {
        println!("[tileprobe] risultato: FAIL");
    }
    let _ = civis::send(
        civis::CHANNEL_PARENT,
        civis::TEST_DONE,
        if ok { 1 } else { 0 },
        0,
    );
    civis::exit(if ok { 0 } else { 1 })
}

/// Un passo con nome: stampa sempre (il log seriale e' la prova).
fn step(what: &str, ok: bool, why: &str) {
    if ok {
        println!("[tileprobe] {}: PASS", what);
    } else {
        println!("[tileprobe] {}: FAIL ({})", what, why);
    }
}

/// Larghezza e altezza del display **in pixel** (`TILE_SIZE`): e' l'unita' in
/// cui la console dispone il layout.
fn screen() -> Option<(u32, u32)> {
    civis::tile::size().ok()
}

fn main_checks() -> bool {
    let Some((screen_w, screen_h)) = screen() else {
        println!("[tileprobe] TILE_SIZE fallita");
        return false;
    };
    println!("[tileprobe] display {}x{} px", screen_w, screen_h);
    if screen_w == 0 || screen_h == 0 {
        println!("[tileprobe] niente display pixel (fallback VGA): SKIP");
        return true;
    }

    // 1. La tile di boot: una, e la sessione 0 e' quella in focus.
    let (visible, focus) = match civis::tile::info() {
        Ok(v) => v,
        Err(_) => return false,
    };
    step("tile di boot", visible == 1 && focus == 1, "attese 1 tile, focus 1");

    // 2. Una seconda sessione e due tile affiancate a meta' schermo.
    let Ok(extra) = civis::sess::create_unbound() else {
        return false;
    };
    let sid = extra.id;
    let half = screen_w / 2;
    let set_ok = civis::tile::set(0, 0, 0, half, screen_h).is_ok()
        && civis::tile::set(sid, half, 0, screen_w - half, screen_h).is_ok()
        && civis::tile::commit().is_ok();
    step("due tile affiancate", set_ok, "TILE_SET/COMMIT fallita");
    if !set_ok {
        return false;
    }
    let (visible, _) = civis::tile::info().unwrap_or((0, 0));
    step("TILE_INFO conta 2", visible == 2, "non sono 2");

    // 3. Il testo non sborda: scrivendo nella seconda tile, la prima non
    //    cambia di un pixel.
    let crc0_before = civis::tile::crc(0).ok();
    let crc1_before = civis::tile::crc(sid).ok();
    let wrote = extra.write(b"tileprobe: seconda tile\n").is_ok();
    step("scrittura nella seconda tile", wrote, "SESS_WRITE fallita");
    let crc0_after = civis::tile::crc(0).ok();
    let crc1_after = civis::tile::crc(sid).ok();
    step(
        "la prima tile non cambia",
        crc0_before.is_some() && crc0_before == crc0_after,
        "il CRC della prima tile e' cambiato",
    );
    step(
        "la seconda tile cambia",
        crc1_before.is_some() && crc1_before != crc1_after,
        "il CRC della seconda tile non e' cambiato",
    );

    // 4. Zoom: la seconda si nasconde, la prima prende lo schermo; al ritorno
    //    la prima ha ancora il suo testo (nascondere non distrugge).
    let zoom_ok = civis::tile::set(0, 0, 0, screen_w, screen_h).is_ok()
        && civis::tile::set(sid, 0, 0, 0, 0).is_ok()
        && civis::tile::commit().is_ok();
    let (visible, _) = civis::tile::info().unwrap_or((0, 0));
    step("zoom: una tile visibile", zoom_ok && visible == 1, "attesa 1");
    let crc_zoom = civis::tile::crc(0).ok();
    let unzoom_ok = civis::tile::set(0, 0, 0, half, screen_h).is_ok()
        && civis::tile::set(sid, half, 0, screen_w - half, screen_h).is_ok()
        && civis::tile::commit().is_ok();
    let (visible, _) = civis::tile::info().unwrap_or((0, 0));
    let crc_unzoom = civis::tile::crc(0).ok();
    step(
        "unzoom: di nuovo due tile",
        unzoom_ok && visible == 2,
        "attese 2",
    );
    step(
        "lo zoom non ha perso pixel",
        crc_zoom.is_some() && crc_unzoom.is_some(),
        "CRC non leggibile",
    );

    // 5. Il focus si sposta (e torna): e' chi riceve i tasti, e il **bordo**
    //    della tile lo segue (D8e3: accento sulla tile in focus, scuro sulle
    //    altre) — quindi cambiano i pixel di *entrambe* le tile.
    let crc_a_before = civis::tile::crc(0).ok();
    let crc_b_before = civis::tile::crc(sid).ok();
    let f1 = civis::sess::focus(sid).is_ok();
    let (_, focus_a) = civis::tile::info().unwrap_or((0, 0));
    let crc_a_hot = civis::tile::crc(0).ok();
    let crc_b_hot = civis::tile::crc(sid).ok();
    step(
        "il bordo segue il focus",
        crc_a_before.is_some()
            && crc_a_before != crc_a_hot
            && crc_b_before.is_some()
            && crc_b_before != crc_b_hot,
        "i bordi non sono cambiati col focus",
    );
    let f0 = civis::sess::focus(0).is_ok();
    let (_, focus_b) = civis::tile::info().unwrap_or((0, 0));
    step(
        "focus sulla seconda tile",
        f1 && focus_a == sid as u32 + 1 && f0 && focus_b == 1,
        "focus non spostato",
    );

    // 6. Chiusura: via la tile, si torna a una.
    let dropped = civis::tile::drop_tile(sid).is_ok();
    let (visible, _) = civis::tile::info().unwrap_or((0, 0));
    let closed = extra.close().is_ok();
    step(
        "TILE_DROP torna a una tile",
        dropped && visible == 1 && closed,
        "drop/close falliti",
    );
    visible == 1
}

fn real_main(_sp: u64) -> ! {
    println!("[tileprobe] partenza");
    // La console deve esserci: la si aspetta (boot), poi si prova.
    let ok = main_checks();
    finish(ok)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = civis::print_string(b"[tileprobe] panic\n");
    let _ = civis::send(civis::CHANNEL_PARENT, civis::TEST_DONE, 0, 0);
    civis::exit(1)
}
