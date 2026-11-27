#include "RunProfileEditor.h"

#include "core/LaunchConfig.h"
#include "widgets/Text.h"

#include <QPlainTextEdit>
#include <QSignalBlocker>
#include <QVBoxLayout>

RunProfileEditor::RunProfileEditor(int visibleLines, QWidget* parent)
    : QWidget(parent)
    , m_text(new QPlainTextEdit)
    , m_status(new Label({}, Label::Style::Caption, Theme::Role::Text3))
{
    auto* v = new QVBoxLayout(this);
    v->setContentsMargins(0, 0, 0, 0);
    v->setSpacing(8);
    m_text->setObjectName(QStringLiteral("runProfileText"));
    m_text->setFont(Theme::mono(12));
    m_text->setLineWrapMode(QPlainTextEdit::NoWrap);
    m_text->setTabChangesFocus(true);
    m_text->setPlaceholderText(QStringLiteral("XCL_EXAMPLE=1"));
    m_text->document()->setDocumentMargin(8);
    m_text->setFixedHeight(QFontMetrics(m_text->font()).lineSpacing() * visibleLines + 20);
    v->addWidget(m_text);
    m_status->setObjectName(QStringLiteral("runProfileStatus"));
    m_status->setWordWrap(true);
    v->addWidget(m_status);

    connect(m_text, &QPlainTextEdit::textChanged, this, [this] {
        if (check())
            emit edited(m_lines);
    });
    check();
}

void RunProfileEditor::setLines(const QStringList& lines)
{
    if (lines == m_lines && LaunchConfig::parseProfile(m_text->toPlainText()).ok())
        return;
    {
        const QSignalBlocker quiet(m_text);
        m_text->setPlainText(lines.join(QLatin1Char('\n')));
    }
    check();
}

bool RunProfileEditor::check()
{
    const LaunchConfig::ProfileParse parsed = LaunchConfig::parseProfile(m_text->toPlainText());
    if (!parsed.ok()) {
        m_status->setText(parsed.errors.join(QLatin1Char('\n')));
        m_status->setRole(Theme::Role::Danger);
        return false;
    }
    m_lines = parsed.lines();
    const qsizetype n = parsed.entries.size();
    QStringList text{n == 1 ? QStringLiteral("1 variable.") : QStringLiteral("%1 variables.").arg(n)};
    for (const auto& e : parsed.entries) {
        if (const QString owner = LaunchConfig::settingOwning(e.first); !owner.isEmpty())
            text << QStringLiteral("%1 comes from Settings (%2), so this line is not used.").arg(e.first, owner);
    }
    m_status->setText(text.join(QLatin1Char(' ')));
    m_status->setRole(text.size() > 1 ? Theme::Role::Warning : Theme::Role::Text3);
    return true;
}
