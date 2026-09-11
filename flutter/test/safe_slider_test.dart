import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common/widgets/safe_slider.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets(
    'Windows pushed route does not create a Slider overlay portal',
    (tester) async {
      final semantics = tester.ensureSemantics();
      await tester.pumpWidget(
        MaterialApp(
          home: Builder(
            builder: (context) => TextButton(
              onPressed: () => Navigator.of(context).push(
                MaterialPageRoute<void>(
                  builder: (_) => Scaffold(
                    body: SafeSlider(
                      value: 0.5,
                      semanticLabel: 'Playback position',
                      onChanged: (_) {},
                    ),
                  ),
                ),
              ),
              child: const Text('Open'),
            ),
          ),
        ),
      );

      await tester.tap(find.text('Open'));
      await tester.pumpAndSettle();

      expect(find.byType(Slider), findsNothing);
      expect(find.byType(OverlayPortal), findsNothing);
      expect(find.bySemanticsLabel('Playback position'), findsOneWidget);
      expect(tester.takeException(), isNull);
      semantics.dispose();
    },
    variant: TargetPlatformVariant.only(TargetPlatform.windows),
  );

  testWidgets(
    'Windows slider supports pointer and keyboard changes',
    (tester) async {
      var value = 0.5;
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: StatefulBuilder(
              builder: (context, setState) => SizedBox(
                width: 300,
                child: SafeSlider(
                  value: value,
                  divisions: 10,
                  onChanged: (next) => setState(() => value = next),
                ),
              ),
            ),
          ),
        ),
      );

      await tester.tapAt(tester.getCenter(find.byType(SafeSlider)));
      await tester.pump();
      await tester.sendKeyEvent(LogicalKeyboardKey.arrowRight);
      await tester.pump();
      expect(value, closeTo(0.6, 0.000001));

      await tester.drag(find.byType(SafeSlider), const Offset(100, 0));
      await tester.pump();
      expect(value, greaterThan(0.6));
      expect(tester.takeException(), isNull);
    },
    variant: TargetPlatformVariant.only(TargetPlatform.windows),
  );

  testWidgets(
    'other platforms retain the Material slider',
    (tester) async {
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(body: SafeSlider(value: 0.5, onChanged: (_) {})),
        ),
      );

      expect(find.byType(Slider), findsOneWidget);
    },
    variant: TargetPlatformVariant.only(TargetPlatform.macOS),
  );
}
