// mathpng — batch LaTeX-math -> transparent PNG renderer for ankimarkable.
//
// Amortizes MicroTeX init across many expressions. Reads stdin, one request
// per line, TAB-separated:  <hash>\t<textSize>\t<tex>
//   <hash>     opaque cache-key token, echoed back + used as output filename
//   <textSize> float point-size for the math (e.g. 40)
//   <tex>      raw math-mode LaTeX body (NO surrounding \(..\) delimiters)
//
// For each line: render at textSize, write <outdir>/<hash>.png with a
// TRANSPARENT background and dark ink (0xff222222), at microtex's native
// render dims (no autocrop — ankimarkable autocrops).
//
// Emits one stdout line per request:  <hash>\t<baseline>\t<width>\t<height>
//   <baseline>  PIXELS from TOP of image down to the math baseline
//               (= getBaseline() fraction * height). descent = height-baseline.
//   <width>/<height>  PNG pixel dims.
// On parse/render failure:  <hash>\tERR\t0\t0  (no file, batch continues).
// EOF on stdin -> flush, exit 0.
//
// Env:
//   MATHPNG_RES        microtex res-fonts dir (default on-device path below)
//   QT_QPA_PLATFORM    must be offscreen|minimal on-device (set by caller)

#include "latex.h"
#include "platform/qt/graphic_qt.h"
#include <QGuiApplication>
#include <QImage>
#include <QColor>
#include <QPainter>
#include <QString>
#include <QByteArray>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <iostream>

using namespace tex;

static const char* DEFAULT_RES = "/home/root/xovi/exthome/textBoxes/microtex-res";
static const color INK = 0xff222222;   // near-black card ink
static const int   LAYOUT_WIDTH = 4000; // wide enough that a single formula never wraps

int main(int argc, char** argv) {
    QGuiApplication app(argc, argv);   // needed for QFont/QPainter text paths

    if (argc < 2) {
        std::fprintf(stderr, "usage: mathpng <outdir>   (requests on stdin)\n");
        return 2;
    }
    std::string outdir = argv[1];
    if (!outdir.empty() && outdir.back() == '/') outdir.pop_back();

    const char* resEnv = std::getenv("MATHPNG_RES");
    std::string res = (resEnv && *resEnv) ? resEnv : DEFAULT_RES;

    try {
        LaTeX::init(res);
    } catch (const std::exception& e) {
        std::fprintf(stderr, "[mathpng] LaTeX::init(%s) failed: %s\n", res.c_str(), e.what());
        return 3;
    }

    std::string line;
    while (std::getline(std::cin, line)) {
        if (line.empty()) continue;
        // strip a trailing CR (in case of CRLF input)
        if (line.back() == '\r') line.pop_back();

        // split into exactly 3 fields on the first two TABs
        std::string::size_type t1 = line.find('\t');
        std::string::size_type t2 = (t1 == std::string::npos)
                                        ? std::string::npos
                                        : line.find('\t', t1 + 1);
        if (t1 == std::string::npos || t2 == std::string::npos) {
            // malformed line: can't recover a hash reliably; skip
            std::fprintf(stderr, "[mathpng] malformed line (need 2 tabs): %s\n", line.c_str());
            continue;
        }
        std::string hash    = line.substr(0, t1);
        std::string sizeStr = line.substr(t1 + 1, t2 - (t1 + 1));
        std::string tex     = line.substr(t2 + 1);

        float textSize = std::atof(sizeStr.c_str());
        if (!(textSize > 0.f)) textSize = 40.f;

        TeXRender* r = nullptr;
        try {
            std::wstring wtex = QString::fromUtf8(QByteArray::fromStdString(tex)).toStdWString();
            r = LaTeX::parse(wtex, LAYOUT_WIDTH, textSize, textSize / 3.f, INK);
            int w = r->getWidth();
            int h = r->getHeight();
            if (w < 1) w = 1;
            if (h < 1) h = 1;

            // Italic glyphs (a lone italic F, slanted letters) overhang microtex's
            // advance-width box on the right, so drawing into a w-wide canvas clips
            // the ink. Pad horizontally and draw offset; height is unchanged so the
            // baseline fraction still holds. The Rust caller crops to the ink, so the
            // extra transparent margin costs nothing downstream.
            int padx = (int)(textSize * 0.18f + 0.5f);
            if (padx < 6) padx = 6;
            int iw = w + 2 * padx;

            QImage img(iw, h, QImage::Format_ARGB32_Premultiplied);
            img.fill(Qt::transparent);
            {
                QPainter p(&img);
                p.setRenderHint(QPainter::Antialiasing, true);
                p.setRenderHint(QPainter::TextAntialiasing, true);
                p.setRenderHint(QPainter::SmoothPixmapTransform, true);
                Graphics2D_qt g2(&p);
                r->draw(g2, padx, 0);
                p.end();
            }

            std::string outPath = outdir + "/" + hash + ".png";
            if (!img.save(QString::fromStdString(outPath), "PNG")) {
                std::fprintf(stderr, "[mathpng] save failed: %s\n", outPath.c_str());
                std::printf("%s\tERR\t0\t0\n", hash.c_str());
                std::fflush(stdout);
                delete r;
                continue;
            }

            // baseline fraction (from top) -> pixels from top
            float frac = r->getBaseline();
            if (frac < 0.f) frac = 0.f;
            if (frac > 1.f) frac = 1.f;
            int baseline = (int)(frac * (float)h + 0.5f);

            std::printf("%s\t%d\t%d\t%d\n", hash.c_str(), baseline, iw, h);
            std::fflush(stdout);
            delete r;
        } catch (const std::exception& e) {
            std::fprintf(stderr, "[mathpng] render error for %s: %s\n", hash.c_str(), e.what());
            std::printf("%s\tERR\t0\t0\n", hash.c_str());
            std::fflush(stdout);
            if (r) delete r;
        } catch (...) {
            std::fprintf(stderr, "[mathpng] render error for %s: unknown\n", hash.c_str());
            std::printf("%s\tERR\t0\t0\n", hash.c_str());
            std::fflush(stdout);
            if (r) delete r;
        }
    }

    LaTeX::release();
    return 0;
}
