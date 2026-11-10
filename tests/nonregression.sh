#!/usr/bin/env bash
# Tests de non-régression de amv-converter.
# Prérequis : ffmpeg + ffprobe dans le PATH. Lancer depuis la racine du dépôt:
#   bash tests/nonregression.sh            (binaire target/release)
#   BIN=/usr/bin/amv-converter bash tests/nonregression.sh   (binaire installé)
set -u

BIN=${BIN:-./target/release/amv-converter}
if [ ! -x "$BIN" ]; then echo "binaire introuvable: $BIN"; exit 2; fi
command -v ffmpeg >/dev/null && command -v ffprobe >/dev/null || { echo "ffmpeg/ffprobe requis"; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
FAILS=0
ok() { echo "  [OK] $*"; }
ko() { echo "  [KO] $*"; FAILS=$((FAILS+1)); }
has() { if [ $# -ge 2 ]; then grep -q -- "$1" "$2"; else grep -q -- "$1"; fi; }

echo "== 0. version & détection GPU =="
V=$("$BIN" --version 2>&1)
case "$V" in
  "amv-converter "*) ok "--version → $V" ;;
  *) ko "--version inattendu: $V" ;;
esac
GPU=$("$BIN" --list-gpu 2>&1)
[ -n "$GPU" ] && ok "--list-gpu → $(echo "$GPU" | tr '\n' ' ')" || ko "--list-gpu vide"

echo "== 1. fixtures =="
# 60 s, deux pistes audio : eng (idx 1) puis fra (idx 2) → l'app doit choisir la 2.
ffmpeg -y -v error -f lavfi -i testsrc2=size=1280x720:rate=25:duration=60 \
       -f lavfi -i sine=frequency=440:duration=60 \
       -f lavfi -i sine=frequency=880:duration=60 \
       -map 0:v -map 1:a -map 2:a -c:v libx264 -preset ultrafast -c:a aac \
       -metadata:s:a:0 language=eng -metadata:s:a:1 language=fra \
       "$WORK/deux_audios.mp4" || { echo "fixture KO"; exit 2; }
# Vidéo sans aucune piste audio.
ffmpeg -y -v error -f lavfi -i testsrc2=size=640x480:rate=25:duration=30 \
       -c:v libx264 -preset ultrafast "$WORK/muet.mp4" || exit 2
ok "fixtures générées"

echo "== 2. conversion CPU + progression =="
OUT=$("$BIN" --batch --gpu cpu "$WORK/deux_audios.mp4" 2>&1); RC=$?
echo "$OUT" | sed 's/^/     /'
[ $RC -eq 0 ] && ok "code de sortie 0" || ko "code de sortie $RC"
has "^ACCEL CPU" <<<"$OUT" && ok "--gpu cpu respecté" || ko "ACCEL inattendu"
has "^DONE ok=1 fail=0 cancelled=false" <<<"$OUT" && ok "bilan DONE ok=1" || ko "bilan DONE incorrect"
has "^AUDIO 2" <<<"$OUT" && ok "piste française (idx 2) sélectionnée" || ko "audio français non sélectionné"

# Progression : au moins 2 mesures, croissantes, dernière à 100, restant décroissant.
NP=$(echo "$OUT" | grep -c '^PROGRESS ')
[ "$NP" -ge 2 ] && ok "$NP mesures de progression" || ko "$NP mesure(s) de progression (attendu ≥2)"
PCTS=$(echo "$OUT" | sed -n 's/^PROGRESS pct=\([0-9-]*\).*/\1/p')
MONO=$(echo "$PCTS" | awk 'NR>1 && $1<prev{bad=1} {prev=$1} END{print bad?"no":"yes"}')
[ "$MONO" = yes ] && ok "pourcentage monotone croissant" || ko "pourcentage non monotone: $(echo $PCTS)"
LAST=$(echo "$PCTS" | tail -1)
[ "$LAST" = "100" ] && ok "progression finale = 100 %" || ko "progression finale = $LAST %"
REMS=$(echo "$OUT" | sed -n 's/^PROGRESS pct=[0-9-]* elapsed=[0-9.]* remaining=\([0-9.-]*\).*/\1/p' | grep -v '^-1$')
if [ -n "$REMS" ]; then
  DEC=$(echo "$REMS" | awk 'NR>1 && ($1+0)>(prev+0.05){bad=1} {prev=$1} END{print bad?"no":"yes"}')
  [ "$DEC" = yes ] && ok "temps restant décroissant ($(echo $REMS | tr '\n' ' '))" || ko "temps restant non décroissant"
else
  ko "aucun temps restant calculé (durée non détectée ?)"
fi

echo "== 3. sortie conforme au format AMV =="
F="$WORK/deux_audios_amv.amv"
[ -f "$F" ] && ok "fichier produit: $(basename "$F") ($(stat -c%s "$F") o)" || ko "fichier AMV absent"
P=$(ffprobe -v error -show_entries stream=codec_name,width,height -of default=noprint_wrappers=1 "$F" 2>&1)
has "codec_name=amv" <<<"$P" && ok "codec vidéo = amv" || ko "codec vidéo: $P"
has "codec_name=adpcm_ima_amv" <<<"$P" && ok "codec audio = adpcm_ima_amv" || ko "codec audio manquant"
has "width=160" <<<"$P" && has "height=120" <<<"$P" && ok "résolution 160x120" || ko "résolution: $P"
FPS=$(ffprobe -v error -select_streams v:0 -show_entries stream=avg_frame_rate -of csv=p=0 "$F")
python3 - "$FPS" <<'PY' && ok "images ~30 fps ($FPS)" || ko "fps: $FPS"
import sys
a, _, b = sys.argv[1].partition('/')
sys.exit(0 if b and abs(float(a)/float(b) - 30) < 0.5 else 1)
PY
# Une seule piste audio dans le conteneur (AMV n'en accepte que 2 flux au total).
NA=$(ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 "$F" | grep -c .)
[ "$NA" = "1" ] && ok "1 seul flux audio" || ko "$NA flux audio"

echo "== 4. nommage unique =="
OUT2=$("$BIN" --batch --gpu cpu "$WORK/deux_audios.mp4" 2>&1)
has "deux_audios_amv_2.amv" <<<"$OUT2" && ok "second passage → _2.amv" || ko "nommage non unique"

echo "== 5. option de résolution =="
OUT3=$("$BIN" --batch --gpu cpu --size 176x144 "$WORK/muet.mp4" 2>&1); echo "$OUT3" | sed 's/^/     /'
P3=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$WORK/muet_amv.amv" 2>&1)
[ "$P3" = "176,144" ] && ok "176x144 respecté" || ko "résolution demandée 176x144, obtenu $P3"
has "^OK" <<<"$OUT3" && ok "source sans audio convertie" || ko "échec sans audio"
has "^AUDIO none" <<<"$OUT3" && ok "aucune piste audio détectée" || ko "audio détecté à tort"
# La piste silencieuse doit être présente dans le fichier produit.
SA=$(ffprobe -v error -select_streams a -show_entries stream=codec_name -of csv=p=0 "$WORK/muet_amv.amv" 2>&1)
[ "$SA" = "adpcm_ima_amv" ] && ok "piste silencieuse ajoutée ($SA)" || ko "audio de substitution: $SA"

echo "== 6. accélération GPU =="
for a in vaapi cuda qsv; do
  O=$("$BIN" --batch --gpu "$a" "$WORK/muet.mp4" 2>&1); R=$?
  if [ $R -eq 0 ] && has "^OK" <<<"$O"; then
    NOTE=$(echo "$O" | grep -o 'repli CPU' || true)
    ok "--gpu $a → conversion réussie${NOTE:+ (repli CPU automatique)}"
  else
    ko "--gpu $a → code $R"
  fi
done

echo "== 7. entrée invalide =="
echo "pas une vidéo" > "$WORK/faux.mp4"
O=$("$BIN" --batch --gpu cpu "$WORK/faux.mp4" 2>&1); R=$?
echo "$O" | sed 's/^/     /'
[ $R -ne 0 ] && ok "code de sortie non nul ($R)" || ko "code de sortie 0 sur échec"
has "^FAIL" <<<"$O" && ok "échec signalé" || ko "échec non signalé"

echo "== 8. options invalides =="
"$BIN" --batch --quality abc "$WORK/muet.mp4" >/dev/null 2>&1
[ $? -eq 2 ] && ok "qualité invalide rejetée (code 2)" || ko "qualité invalide acceptée"
"$BIN" --batch --size 0x120 "$WORK/muet.mp4" >/dev/null 2>&1
[ $? -eq 2 ] && ok "résolution invalide rejetée (code 2)" || ko "résolution invalide acceptée"

echo "== 9. démarrage de l'interface (offscreen) =="
if command -v timeout >/dev/null; then
  QT_QPA_PLATFORM=offscreen timeout 5 "$BIN" "$WORK/muet.mp4" >/dev/null 2>"$WORK/gui.err"; R=$?
  if [ $R -eq 124 ]; then ok "fenêtre construite, boucle d'événements active"
  elif [ $R -eq 139 ]; then ko "segfault au démarrage"; cat "$WORK/gui.err"
  else ko "sortie prématurée (code $R)"; cat "$WORK/gui.err"; fi
fi

echo
if [ "$FAILS" -eq 0 ]; then echo "TOUS LES TESTS PASSENT"; exit 0; fi
echo "$FAILS test(s) en échec"; exit 1
