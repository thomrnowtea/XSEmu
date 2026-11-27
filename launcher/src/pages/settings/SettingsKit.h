#pragma once

#include "theme/Theme.h"

#include <QComboBox>

#include <functional>
#include <QWidget>

class Label;
class QHBoxLayout;
class QVBoxLayout;

// The pieces every Settings section is made of (Settings pages): the section title and description,
// CAPS group labels, and rows with a label, help text and a control on the right over a hairline.
namespace SettingsKit {

// The copy for anything the emulator cannot do yet.
QString notInBuild();

class Row : public QWidget {
    Q_OBJECT
public:
    Row(const QString& label, const QString& help = {}, QWidget* parent = nullptr);
    void addControl(QWidget* w);        // right-aligned, in order
    void setHelp(const QString& help);
    // Disable the controls and say why under the help ("Not in this XSEmu build yet.").
    void setUnavailable(const QString& why = notInBuild());
    void setAvailable(); // undoes setUnavailable
    void setBorder(bool on);
    Label* helpLabel() const { return m_help; }

protected:
    void paintEvent(QPaintEvent*) override;

private:
    QHBoxLayout* m_row;
    Label* m_help;
    Label* m_why;
    bool m_border = true;
};

struct Section {
    QWidget* page;
    QVBoxLayout* body;

    void caps(const QString& text) const;   // "CONNECTION"
    Row* row(const QString& label, const QString& help = {}, QWidget* control = nullptr) const;
    void add(QWidget* w, int spacingBefore = 0) const;
    void finish() const; // the last row loses its hairline; stretch below
};

// An empty title leaves the heading out (About draws its own).
Section makeSection(const QString& title, const QString& description);

// The design's 32 px select: surface, strong hairline, the item's icon (when it has one), 13 px text
// and a chevron. The list is Qt's.
class Dropdown : public QComboBox {
    Q_OBJECT
public:
    using QComboBox::QComboBox;

protected:
    void paintEvent(QPaintEvent*) override;
};

// 180×32 drop-down with (label, value) items.
QComboBox* dropdown(const QList<QPair<QString, QVariant>>& items, const QVariant& current, int width = 180);

// A right-aligned status word ("Succeeds", "Connected").
Label* status(const QString& text, Theme::Role role = Theme::Role::Text2);

// Runs f each time w is shown (a section becoming visible re-reads the disk).
void onShow(QWidget* w, std::function<void()> f);

// A key cap ("F11", "Esc").
QWidget* keyCap(const QString& key);

// A sunken, monospaced path box that elides in the middle and shows the full path as a tooltip.
class PathField : public QWidget {
    Q_OBJECT
public:
    explicit PathField(const QString& path = {}, QWidget* parent = nullptr);
    void setPath(const QString& path);
    QSize sizeHint() const override { return {200, 32}; }

protected:
    void paintEvent(QPaintEvent*) override;

private:
    QString m_path;
};

}
